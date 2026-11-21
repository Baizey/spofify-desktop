#!/usr/bin/env python3
"""Exercise continuous playlist details in an owned native Wayland display.

Build from the worktree root:
  SLINT_EMIT_DEBUG_INFO=1 cargo build --offline --locked --features slint/mcp --example playlist_acceptance
  python3 -B scripts/acceptance/continuous_playlists.py target/debug/examples/playlist_acceptance

Requires Cage and worker-local IPv4 loopback for Slint MCP only. No account,
store, audio engine or external service is used. PNGs and exact action evidence
are saved under a new .tmp/continuous-playlists* directory.
"""
import argparse
import json
from pathlib import Path
import re
import shutil
import sys
from tempfile import TemporaryDirectory
from time import sleep

sys.dont_write_bytecode = True
from fixture import Failure, check, private_env
from native import DOWN, END, ESCAPE, RETURN, bounds, contains, desktop, element_id, element_type, named, wait


COUNT = 1207
DUPLICATES = {100, 101, 1100, 1101, COUNT}  # one-based display numbers
BOUNDARIES = sorted({*range(100, COUNT, 100), 1161})
PAGER = r"First page|Previous page|Next page|Last page|Load more"
TAB = "\t"


def center(element):
    x, y, right, bottom = bounds(element)
    return (x + right) / 2, (y + bottom) / 2


def has_type(element, name):
    return element_type([element], name) is not None


class Review:
    def __init__(self, window, journal):
        self.window = window
        self.journal = journal
        self.expected = []
        self.detail = ""

    def evidence(self, **fields):
        with (self.window.output / "interactions.jsonl").open("a") as log:
            log.write(json.dumps(dict(detail=self.detail, **fields)) + "\n")

    def lines(self):
        return self.journal.read_text().splitlines()

    def expect(self, *fields):
        if fields:
            self.expected.append("\t".join(map(str, fields)))
            wait(f"callback {fields!r}", lambda: len(self.lines()) >= len(self.expected), timeout=3)
        sleep(.12)
        check(self.lines() == self.expected,
              f"unexpected callback(s): expected {self.expected!r}, observed {self.lines()!r}")
        if fields:
            self.evidence(callback=fields)

    def row_action(self, op, number):
        index = number - 1
        self.expect("row", self.detail, op, index,
                    "shared-recording" if number in DUPLICATES else f"track-{index}")

    def viewport(self):
        found = self.window.mcp.value("find_elements_by_id", {
            "windowHandle": self.window.handle, "elementsId": "AppWindow::catalog-scroll",
        }).get("elementHandles", [])
        check(len(found) == 1, "missing or duplicated native catalog scroll viewport")
        handle = found[0]
        return self.window.mcp.value("get_element_properties", {"elementHandle": handle}) | {"handle": handle}

    def visible_rows(self):
        viewport = self.viewport()
        elements = self.window.subtree(viewport, max_elements=1000)
        lines = [e for e in elements if has_type(e, "MediaLine") and contains(viewport, e, slack=1)]
        rows = {}
        for face in elements:
            match = re.fullmatch(r"Song (\d+)(?:, (?:Playing|Current track))?",
                                 face.get("accessibleDescription", ""))
            if not match or face.get("accessibleRole") not in ("Button", "Text"):
                continue
            label = face.get("accessibleLabel", "")
            if face.get("accessibleRole") == "Button" and not label.startswith("Play track: "):
                continue
            line = next((e for e in lines if contains(e, face, slack=1)), None)
            if line:
                number = int(match[1])
                check(number not in rows, f"duplicate accessible face for song {number}")
                rows[number] = (line, face)
        return viewport, dict(sorted(rows.items()))

    def context(self, number):
        return wait(f"fully visible song {number} in {self.detail}",
                    lambda: self.visible_rows()[1].get(number), timeout=3)

    def scroll(self, viewport, delta):
        x, y = center(viewport)
        self.window.mcp.call("dispatch_pointer_scroll", {"windowHandle": self.window.handle,
                             "position": {"x": x, "y": y}, "deltaY": delta})
        sleep(.10)

    def reveal(self, first, last=None):
        last = first if last is None else last
        for _ in range(18):
            viewport, rows = self.visible_rows()
            visible = list(rows)
            if first in rows and last in rows:
                return rows
            check(len(visible) >= 2, f"not enough visible tracks in {self.detail}: {visible}")
            a, b = visible[:2]
            pitch = (bounds(rows[b][0])[1] - bounds(rows[a][0])[1]) / (b - a)
            delta = (a - max(1, first - 2)) * pitch
            check(pitch > 0 and delta != 0, f"cannot reveal songs {first}–{last} from {visible}")
            self.scroll(viewport, max(-5800, min(5800, delta)))
        raise Failure(f"cannot reach songs {first}–{last} in {self.detail} by native scrolling")

    def drag_scrollbar_to_tail(self):
        viewport = self.viewport()
        bar = element_id(self.window.subtree(viewport, max_elements=1000), "vertical-bar")
        check(bar and contains(viewport, bar), "missing outer catalog scrollbar")
        thumb = element_id(self.window.subtree(bar), "thumb")
        check(thumb and contains(bar, thumb), "missing catalog scrollbar thumb")
        x, y = center(thumb)
        target = {"x": x, "y": bounds(bar)[3] - 10}
        check(target["y"] - y > 100, "drag must travel a substantial part of the outer scrollbar")
        self.window.mcp.call("drag_element", {"elementHandle": thumb["handle"], "target": target})
        self.evidence(gesture="catalog-scrollbar-drag", pointer=[x, y], target=target)
        self.context(COUNT)
        self.expect()

    def child(self, number, label):
        line, _ = self.context(number)
        target = named(self.window, self.window.subtree(line), label)
        check(target and contains(line, target), f"song {number}: missing/clipped {label}")
        return target

    def pointer(self, name, number, target, *, double=False):
        x, y = self.window.pointer_position_for_click(name, element=target, double=double)
        left, top, right, bottom = bounds(target)
        check(left <= x < right and top <= y < bottom,
              f"native press {(x, y)} missed {name} at {bounds(target)}")
        self.evidence(gesture=name, song=number, pointer=[x, y], target=bounds(target), presses=2 if double else 1)

    def key(self, key):
        self.window.key(key)
        self.evidence(key=key)

    def label(self, number):
        return "Repeated recording" if number in DUPLICATES else f"Recording {number:04}"

    def face(self, number, *, double=False):
        line, face = self.context(number)
        check(face.get("accessibleLabel") == f"Play track: {self.label(number)}", f"wrong song {number}: {face}")
        # The face center is blank space within the track, separate from the
        # metadata links and distinct Play/overflow control columns.
        self.pointer("face-double" if double else "face", number, face, double=double)
        self.row_action("play", number) if double else self.expect()
        return line

    def alignment(self, numbers, capture):
        self.window.mcp.call("move_pointer", {"windowHandle": self.window.handle, "position": {"x": 5, "y": 5}})
        lefts, rights, play_columns, menu_columns, word_columns = [], [], [], [], []
        for number in numbers:
            line, _ = self.context(number)
            l, t, r, b = bounds(line)
            lefts.append(l)
            rights.append(r)
            words = element_id(self.window.subtree(line), "words")
            check(words and contains(line, words), f"song {number}: missing/clipped title and artist column")
            word_columns.append(bounds(words)[0])
            patterns = [f"Play {self.label(number)}"]
            if self.detail != "saved-collection":
                patterns.append(f"Show actions for {self.label(number)}")
            for pattern, columns in zip(patterns, (play_columns, menu_columns)):
                control = self.child(number, pattern)
                x, y = center(control)
                check(abs(y - (t + b) / 2) <= 1, f"song {number}: {pattern} not vertically centered")
                columns.append(x)
        for edges in (lefts, rights, word_columns, play_columns, menu_columns):
            check(not edges or max(edges) - min(edges) <= 1, f"playlist columns drift: {edges!r}")
        self.evidence(alignment=dict(songs=numbers, lefts=lefts, rights=rights, words=word_columns,
                                     play=play_columns, menu=menu_columns))
        self.window.pixels(capture)

    def boundaries(self, numbers):
        for number in numbers:
            rows = self.reveal(number, number + 1)
            earlier, later = (bounds(rows[i][0]) for i in (number, number + 1))
            gap = later[1] - earlier[3]
            check(abs(earlier[0] - later[0]) <= 1 and 0 <= gap <= 3,
                  f"old boundary {number}/{number + 1} is not one continuous list: {earlier}, {later}")
            self.evidence(boundary=[number, number + 1], boxes=[earlier, later])
        self.expect()

    def open_detail(self, kind, name, parent, index):
        self.window.click(f"Open {name}")
        self.expect("row", parent, "open", index, "")
        self.detail = kind
        self.window.title(name)
        self.window.find("Back", roles=("Button",))
        self.window.absent(PAGER, roles=("Button",))
        self.context(1)

    def manual(self):
        self.open_detail("playlist-manual", "Synthetic Manual", "playlists", 0)
        self.boundaries([100])
        self.alignment([100, 101, 102], "01-manual-old-boundary")
        self.face(100)
        self.key(DOWN)
        self.key(RETURN)  # Cross the former page boundary by keyboard.
        self.row_action("play", 101)
        sleep(.60)  # Separate the focus-only click from the next double click.
        self.face(101, double=True)
        play = self.child(100, "Play Repeated recording")
        self.pointer("play-button-double", 100, play, double=True)
        self.row_action("play", 100)

        self.face(101)
        # Real Tab traversal from the face, through any metadata/Play button,
        # onto the row overflow. The following Return opens it, not playback.
        # The fixture supplies one album link between the face and Play.
        for _ in range(3):
            self.key(TAB)
        self.key(RETURN)
        self.window.find("Hide actions for Repeated recording", roles=("Button",))
        self.expect()
        self.window.pixels("02-manual-keyboard-menu")
        self.key(DOWN)
        self.key(RETURN)
        self.row_action("add", 101)
        self.window.absent("Hide actions for Repeated recording", roles=("Button",))

        self.boundaries(BOUNDARIES[1:])
        self.reveal(COUNT)
        self.alignment([1205, 1206, COUNT], "03-manual-last")
        self.pointer("last-play", COUNT, self.child(COUNT, "Play Repeated recording"))
        self.row_action("play", COUNT)

        self.reveal(1100, 1101)
        before = bounds(self.context(1100)[0])
        album_link = self.child(1100, "Open album: Synthetic Album")
        self.pointer("album-link", 1100, album_link)
        self.expect("page", self.detail, "album", "fixture-album")
        self.window.title("Synthetic Album")
        def delayed_back(*, capture=False):
            self.window.click("Back")
            self.expect("navigate", "album", "back")
            self.window.title("Playlist", timeout=2)
            self.window.absent(r"Play track: .*", roles=("Button",))
            if capture:
                self.window.pixels("04-manual-back-loading")
            check(bounds(self.context(1100)[0]) == before, "Back through loading did not restore playlist scroll")
            sleep(.05)  # Allow the remounted delegate to finish restoring the link focus.

        delayed_back(capture=True)
        self.key(RETURN)
        self.expect("page", self.detail, "album", "fixture-album")
        self.window.title("Synthetic Album")
        delayed_back()
        self.key(TAB)  # From the restored album link to this occurrence's Play.
        self.key(RETURN)
        self.row_action("play", 1100)
        first_browse_row = next(iter(self.visible_rows()[1]))

        self.window.click("Edit order")
        self.window.find("Done", roles=("Button",))
        self.window.pixels("04-manual-edit-order")

        def editor_indices():
            elements = self.window.elements()
            editor = element_id(elements, "playlist-order")
            if not editor:
                return None
            items = self.window.subtree(editor, max_elements=1000)
            viewport = element_id(items, "list")
            if not viewport:
                return None
            visible = [(e.get("accessibleItemIndex"), e.get("accessibleLabel"), bounds(e)) for e in items
                       if e.get("accessibleRole") == "ListItem" and contains(viewport, e)]
            return visible or None

        visible = wait("visible playlist editor rows", editor_indices, timeout=3)
        self.evidence(order_rows=visible, first_full_browse_row=first_browse_row)
        # The edit view starts at the leading browse row, which may be partly
        # clipped. Its cards have a different height from the normal rows.
        leading = {first_browse_row - 1, first_browse_row - 2}
        check(any(index in leading for index, _, _ in visible[:2]),
              f"editor did not open at the deep browse position near {first_browse_row}: {visible}")
        self.expect()
        self.window.click("Done")
        self.window.absent("Done", roles=("Button",))
        check(bounds(self.context(1100)[0]) == before, "Edit order / Done changed playlist scroll")
        self.window.pixels("05-manual-order-done")
        self.window.click("Back")
        self.expect("navigate", self.detail, "back")
        self.window.find("Open Synthetic Manual", roles=("Button",))
        print("PASS manual continuous boundaries, last row, exact pointer/keyboard play and menu, Back and Done", flush=True)

    def rules(self):
        self.open_detail("playlist-rules", "Synthetic Rules", "playlists", 1)
        self.boundaries([100, 1100, 1200, 100])
        self.face(101)
        self.drag_scrollbar_to_tail()
        # The focused duplicate was evicted. Return cannot activate it, but
        # navigation must still work without first pointing at a different row.
        self.key(RETURN)
        self.expect()
        self.key(END)
        self.key(RETURN)
        self.row_action("play", COUNT)
        self.alignment([1205, 1206, COUNT], "06-rules-last")
        self.face(COUNT, double=True)
        self.pointer("rules-overflow", COUNT, self.child(COUNT, "Show actions for Repeated recording"))
        self.window.find("Hide actions for Repeated recording", roles=("Button",))
        self.expect()
        self.window.click("Add to queue · Repeated recording")
        self.row_action("queue", COUNT)
        self.window.click("Back")
        self.expect("navigate", self.detail, "back")
        self.window.find("Open Synthetic Rules", roles=("Button",))
        print("PASS rules continuous boundaries, exact last-row play/menu and Back", flush=True)

    def saved(self):
        self.window.click("Saved offline")
        self.expect("navigate", "playlists", "offline")
        self.open_detail("saved-collection", "Synthetic Saved", "offline", 0)
        self.boundaries([100])
        self.reveal(99, 101)
        _, unavailable = self.context(99)
        check(unavailable.get("accessibleRole") == "Text"
              and unavailable.get("accessibleLabel") == "Unavailable song", "Saved unavailable slot became playable")
        self.alignment([100, 101, 102], "07-saved-old-boundary")
        self.face(101, double=True)
        self.boundaries([1100, 1200])
        self.face(1201)
        self.key(END)
        self.context(COUNT)
        self.key(RETURN)
        self.row_action("play", COUNT)
        self.alignment([1205, 1206, COUNT], "08-saved-last")
        self.key(ESCAPE)
        self.expect("navigate", self.detail, "back")
        self.window.find("Open Synthetic Saved", roles=("Button",))
        print("PASS Saved continuous boundaries, unavailable occurrence, exact last-row play and Back", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("fixture", type=Path, help="prebuilt MCP-enabled playlist_acceptance example")
    parser.add_argument("--output", default="continuous-playlists", help="new directory name under worktree .tmp/")
    args = parser.parse_args()
    binary = args.fixture.resolve(strict=True)
    root = Path(__file__).resolve().parents[2]
    check(Path.cwd() == root, "run from the desktop worktree root")
    check(re.fullmatch(r"continuous-playlists(?:-[a-zA-Z0-9_-]+)?", args.output), "use a continuous-playlists* output name")
    base = root / ".tmp"
    check(not base.is_symlink(), "refusing symlinked .tmp directory")
    base.mkdir(mode=0o700, exist_ok=True)
    output = base / args.output
    output.mkdir(mode=0o700)
    journal = output / "callbacks.tsv"
    print(f"Native evidence: {output}", flush=True)
    with TemporaryDirectory(prefix="spofify-playlist-") as tmp:
        runtime = Path(tmp)
        env = dict(private_env(runtime), SPOFIFY_PLAYLIST_FIXTURE_JOURNAL=str(journal))
        env["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={runtime / 'runtime' / 'no-bus'}"
        try:
            with desktop(binary, runtime, env, output) as window:
                check(window.window_properties()["scaleFactor"] == 1, "expected a scale-1 native display")
                review = Review(window, journal)
                review.manual()
                review.rules()
                review.saved()
                review.expect()
        finally:
            log = runtime / "desktop.log"
            if log.exists():
                shutil.copyfile(log, output / "desktop.log")


if __name__ == "__main__":
    try:
        main()
    except (Failure, TimeoutError) as error:
        raise SystemExit(f"FAIL {error}") from error
