#!/usr/bin/env python3
"""Run native playback layout and focus checks without sign-in or audio (requires Pillow).

Build with SLINT_EMIT_DEBUG_INFO=1 cargo build --locked --features slint/mcp
    --example playback_acceptance
Run with python3 scripts/acceptance/playback_polish.py target/debug/examples/playback_acceptance
"""
import argparse
from base64 import b64decode
from io import BytesIO
from pathlib import Path
import re
import shutil
import sys
from tempfile import TemporaryDirectory

from PIL import Image, ImageChops

sys.dont_write_bytecode = True
from fixture import Failure, check, private_env
from native import (COMPACT, DESKTOP, ESCAPE, MINIMUM, RETURN, ULTRAWIDE, WIDE,
                    assert_dock_layout, assert_queue_columns, assert_transport, bounds,
                    contains, desktop, element_id, element_type, named, wait)


TAB = "\t"
# Base and fixed-dark default keyboard indicators. Sample an edge, not text or selection.
FOCUS_INK = {(145, 191, 255), (223, 189, 120)}


def frame(window):
    parts = window.mcp.call("take_screenshot", {"windowHandle": window.handle})["content"]
    data = next(part["data"] for part in parts if part["type"] == "image")
    with Image.open(BytesIO(b64decode(data, validate=True))) as image:
        check(image.size == tuple(map(round, window.dimensions())), "expected a scale-1 fixture window")
        return image.convert("RGB")


def move_off(window):
    window.mcp.call("move_pointer", {"windowHandle": window.handle, "position": {"x": 5, "y": 5}})


def button(window, label):
    return window.find(label, roles=("Button",))


def rect_pixels(image, element):
    x, y, right, bottom = map(round, bounds(element))
    return [(px, py, image.getpixel((px, py))) for py in range(max(0, y), min(image.height, bottom))
            for px in range(max(0, x), min(image.width, right))]


def has_ring(image, control):
    left, top, right, _ = map(round, bounds(control))
    return sum(image.getpixel((x, y)) in FOCUS_INK for x in range(left + 9, right - 9)
               for y in range(top, top + 3)) >= 6


def ring(window, label, visible):
    control = button(window, label)
    image = frame(window)
    check(has_ring(image, control) == visible,
          f"{label}: expected {'a visible keyboard ring' if visible else 'no pointer ring'}")
    if label in ("Play", "Pause") and not visible:
        # The fixed-dark theme's inner separator is dark, not gold. It too
        # must be absent when the primary action was only clicked.
        left, top, right, _ = bounds(control)
        cx, y = round((left + right) / 2), round(top)
        check(image.getpixel((cx, y + 3)) == image.getpixel((cx, y + 7)),
              f"{label}: focus separator remained without a keyboard ring")


def focus_checks(window):
    window.click("Play queue")
    move_off(window)
    ring(window, "Close play queue", False)
    window.key(TAB)
    pane = element_id(window.elements(), "queue-pane")
    shuffle = named(window, window.subtree(pane, max_elements=1000), "Shuffle")
    image = frame(window)
    check(shuffle and has_ring(image, shuffle), "Tab from the pointer-opened queue did not show keyboard focus")
    check(not has_ring(image, button(window, "Close play queue")), "old queue ring remained after Tab")

    window.key(ESCAPE)
    window.absent("Close play queue", roles=("Button",))
    ring(window, "Play queue", True)
    window.key(RETURN)
    ring(window, "Close play queue", True)
    window.click("Close play queue")
    move_off(window)
    window.absent("Close play queue", roles=("Button",))
    ring(window, "Play queue", False)
    window.key(TAB)
    ring(window, "Playback devices", True)
    ring(window, "Play queue", False)
    print("PASS pointer queue open/close is quiet; Tab and keyboard open/Escape move visible focus", flush=True)


def painted_transport(window, transport, surface):
    assert_transport(window, transport, surface)
    elements = window.subtree(transport)
    glyphs = [e for e in elements if element_type([e], "Glyph")]
    picture = frame(window)
    centers = []
    for pattern in ("Shuffle", "Previous track", "Play|Pause", "Next track", "Repeat"):
        button = named(window, elements, pattern)
        glyph = next(e for e in glyphs if contains(button, e))
        points = [(x, y) for x, y, rgb in rect_pixels(picture, glyph)
                  if (max(rgb) < 95 if pattern == "Play|Pause" else min(rgb) > 180)]
        check(len(points) > 10, f"{surface} {pattern} glyph isn't visible in its target")
        xs, ys = zip(*points)
        actual_x, actual_y = (min(xs) + max(xs) + 1) / 2, (min(ys) + max(ys) + 1) / 2
        x, y, right, bottom = bounds(button)
        check(abs(actual_x - (x + right) / 2) <= 2.5 and abs(actual_y - (y + bottom) / 2) <= 1,
              f"{surface} {pattern} painted glyph is off center: {(actual_x, actual_y)} in {bounds(button)}")
        centers.append(actual_y)
    check(max(centers) - min(centers) <= 1, f"{surface} painted glyph centers differ: {centers}")
    print(f"PASS {surface}: all five painted pictograms share a centerline", flush=True)


def queue_pixels(window):
    assert_queue_columns(window)
    pane = element_id(window.elements(), "queue-pane")
    elements = window.subtree(pane, max_elements=1000)
    scroll = element_id(elements, "queue-scroll")
    image = frame(window)
    columns = {}
    for row in elements:
        if not element_type([row], "MediaLine") or not contains(scroll, row):
            continue
        content = window.subtree(row)
        label = next((e["accessibleLabel"] for e in content
                      if window.matches(e, r"(?:Play )?Queue recording [012]")), None)
        check(label, "missing accessible queue row label")
        if label == "Queue recording 1":
            cover = element_type(content, "Cover")
            check(cover and any(rgb == (75, 53, 94) for _, _, rgb in rect_pixels(image, cover)),
                  "fixture's current queue row did not paint artwork")
        words = element_id(content, "words")
        lines = [e for e in window.subtree(words) if element_type([e], "Text")]
        check(len(lines) == 3, f"expected title/artist/album in a visible queue row: {bounds(words)}")
        starts = []
        for line in lines:
            ink = [x for x, _, color in rect_pixels(image, line) if max(color) >= 170]
            check(ink, f"no legible queue text at {bounds(line)}")
            starts.append(min(ink))
        check(max(starts) - min(starts) <= 1, f"queue title/artist/album paint misaligned: {starts}")
        columns[label] = starts
    expected = {"Queue recording 1", "Play Queue recording 2"}
    if window.dimensions()[0] == ULTRAWIDE[0]:
        expected.add("Queue recording 0")
    check(expected <= columns.keys() and all(starts == columns["Queue recording 1"] for starts in columns.values()),
          f"current and compact row paint misaligned: {columns}")
    print(f"PASS queue painted title/artist/album starts: {columns} at {window.dimensions()[0]:g}px", flush=True)


def live_sliders(window):
    window.find("0:46", roles=("Text",))
    window.drag_slider(0.72)
    window.find(r"[12]:\d{2}", roles=("Text",))
    button(window, "Unmute desktop playback")
    slider = window.find("Desktop volume", roles=("Slider",))
    x, y, right, bottom = bounds(slider)
    window.mcp.call("drag_element", {"elementHandle": slider["handle"],
                                    "target": {"x": x + (right - x) * 0.24, "y": (y + bottom) / 2}})
    button(window, "Mute desktop playback")
    print("PASS seek and volume drags reach their UI callbacks", flush=True)


def run(window):
    for mode in (MINIMUM, COMPACT, (819, 640), (820, 640), WIDE, DESKTOP, ULTRAWIDE):
        window.resize(mode)
        assert_dock_layout(window)
        if mode == ULTRAWIDE:
            queue_pixels(window)
            dock = element_type(window.elements(), "PlayerBar")
            transport = element_type(window.subtree(dock, max_elements=1000), "Transport")
            painted_transport(window, transport, "ultrawide dock (pause)")
        if mode == MINIMUM:
            live_sliders(window)
            window.click("Play queue")
            queue_pixels(window)
            window.click("Close play queue")

    window.resize(COMPACT)
    focus_checks(window)
    dock = element_type(window.elements(), "PlayerBar")
    transport = element_type(window.subtree(dock, max_elements=1000), "Transport")
    painted_transport(window, transport, "compact dock (pause)")
    window.click("Previous track")
    window.absent("Playback position", roles=("Slider",))
    no_seek = element_type(window.elements(), "PlayerBar")
    check(bounds(no_seek)[1] > bounds(dock)[1], "compact dock did not release the unused seek lane")
    window.click("Next track")
    title = "Now playing: Synthetic Tone 002"
    button(window, title)
    assert_dock_layout(window)
    window.click("Next track")
    long_title = title + " — with a longer title"
    title_box = tuple(map(round, bounds(button(window, long_title))))
    move_off(window)
    long_frame = frame(window).crop(title_box)
    window.click("Next track")
    button(window, title)
    move_off(window)
    difference = ImageChops.difference(long_frame, frame(window).crop(title_box)).getbbox()
    check(difference and difference[2] - difference[0] >= 12, "dock title fixture did not visibly change")
    window.click("Pause")
    move_off(window)
    ring(window, "Play", False)
    painted_transport(window, transport, "compact dock (play)")
    window.click(title)
    page = wait("Now Playing after pointer activation", lambda: element_type(window.elements(), "NowPlaying"))
    window.absent(re.escape(title), roles=("Button",))
    window.scroll_catalog(-150)
    transport = element_type(window.subtree(page, max_elements=1000), "Transport")
    move_off(window)
    painted_transport(window, transport, "compact Now Playing (play)")
    ring(window, "Play", False)
    window.click("Play")
    move_off(window)
    ring(window, "Pause", False)
    painted_transport(window, transport, "compact Now Playing (pause)")
    window.key(" ")
    ring(window, "Play", True)
    window.key(TAB)
    ring(window, "Play", False)
    ring(window, "Next track", True)
    window.key(ESCAPE)
    button(window, title)
    print("PASS dock title changes, navigates by pointer, hides, and returns on Back", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("example_binary", type=Path, help="prebuilt disposable playback_acceptance example")
    args = parser.parse_args()
    with TemporaryDirectory(prefix="spofify-playback-polish-") as name:
        root = Path(name)
        binary = root / "playback-acceptance"
        shutil.copy2(args.example_binary.resolve(strict=True), binary)
        with desktop(binary, root, private_env(root), multisize=True) as window:
            run(window)


if __name__ == "__main__":
    try:
        main()
    except (Failure, OSError, ValueError) as error:
        sys.exit(f"FAIL: {error}")
