#!/usr/bin/env python3
"""Exercise Play through output request, buffering, and playback on an isolated native dock.

Build: SLINT_EMIT_DEBUG_INFO=1 cargo build --offline --locked --features slint/mcp
       --example dock_transport_acceptance
Run:   python3 scripts/acceptance/dock_transport.py target/debug/examples/dock_transport_acceptance
Requires Cage, wlr-randr, and Pillow. No app account, output, or backend service is used.
"""
import argparse
from dataclasses import dataclass
from pathlib import Path
import shutil
import sys
from tempfile import TemporaryDirectory
from time import sleep

from PIL import Image, ImageChops

sys.dont_write_bytecode = True
from fixture import Failure, check, private_env
from native import (COMPACT, MINIMUM, WIDE, assert_disjoint, bounds, contains, desktop, element_id,
                    element_type, named, player_bar, verify, wait)
from playback_polish import TAB, frame, has_ring, move_off


REQUEST = "Connecting this output…"
BUFFERING = "Loading audio…"
ACTIONS = ("Shuffle", "Previous track", "Play|Pause", "Next track", "Repeat")
ACCENT = (102, 227, 180)
METADATA = ("track title", "artist credits", "Open Now Playing")


@dataclass
class Sample:
    dock_handle: dict
    layout: dict
    paint: dict
    image: Image.Image
    status: tuple | None
    clock: tuple | None
    seek_end: int | None


def center(box):
    x, y, right, bottom = box
    return (x + right) / 2, (y + bottom) / 2


def pixels(image, box, predicate):
    x, y, right, bottom = map(round, box)
    check(0 <= x < right <= image.width and 0 <= y < bottom <= image.height,
          f"paint sample outside native frame: {box}")
    return {(px, py) for py in range(y, bottom) for px in range(x, right)
            if predicate(image.getpixel((px, py)))}


def status_ink(rgb):
    # Caption text is antialiased and need not contain an exact theme-color pixel.
    r, g, b = rgb
    return r > 100 and r > g + 10 and g > b + 20


def status_pixels(image, box, transport):
    # A broken layout can put the keyboard focus ring in the caption's bounds.
    left, top, right, bottom = map(round, transport)
    return {(x, y) for x, y in pixels(image, box, status_ink)
            if not (left <= x < right and top <= y < bottom)}


def glyph_paint(image, glyph, button, primary):
    ink = pixels(image, bounds(glyph),
                 (lambda rgb: max(rgb) < 95) if primary else (lambda rgb: min(rgb) > 180))
    label = button["accessibleLabel"]
    check(len(ink) > 10, f"{label} glyph did not paint clearly")
    xs, ys = zip(*ink)
    left, top, _, _ = bounds(glyph)
    cx, cy = center(bounds(button))
    # Compare actual ink, not merely the geometry of the image container.
    actual = center((min(xs), min(ys), max(xs) + 1, max(ys) + 1))
    check(abs(actual[0] - cx) <= 2.5 and abs(actual[1] - cy) <= 1,
          f"{label} painted center {actual} is off target center {(cx, cy)}")
    if primary:
        columns = set(xs)
        bars = sum(x - 1 not in columns for x in columns)
        check(bars == (2 if label == "Pause" else 1), f"{label} painted the wrong pictogram")
    return {(x - round(left), y - round(top)) for x, y in ink}, actual


def capture(window, phase, pictures, *, status="", playing=False, ticked=False, focus=None, known=True):
    label = "Pause" if playing else "Play"
    dock = player_bar(window)
    clock = window.find("Elapsed 1:01" if ticked else "Elapsed 0:46", roles=("Text",), within=dock) if known else None
    caption = window.title(status) if status else None
    window.find(label, roles=("Button",), within=dock)
    if not status:
        window.absent(f"{REQUEST}|{BUFFERING}", roles=("Text",))
    elements = window.elements()
    dock = player_bar(window, elements)
    catalog = element_id(elements, "catalog-scroll")
    check(catalog, "missing catalog viewport")
    children = window.subtree(dock, max_elements=1000)
    title = named(window, children, "Open album for Synthetic Tone 001")
    check(title and title.get("accessibleEnabled", True), "retained track lost its title action")
    output = named(window, children, "Playback devices")
    expected_output = "This desktop" if status == BUFFERING or playing else "No output selected"
    check(output and output.get("accessibleDescription") == expected_output,
          f"expected output description {expected_output!r}")
    transport = element_type(children, "Transport")
    rail = element_type(children, "SeekBar")
    check(transport, "missing transport")
    seek = named(window, children, "Playback position", "Slider")
    stable = {"track information": element_id(children, "track-info"), "track title": title,
              "artist credits": element_type(children, "CreditLine"),
              "Desktop volume": named(window, children, "Desktop volume", "Slider"), "output control": output}
    stable.update((label, named(window, children, label)) for label in
                  ("Lyrics", "Play queue", "Mute desktop playback", "Open Now Playing"))
    for name, item in stable.items():
        check(item and contains(dock, item), f"missing or clipped dock item: {name}")
    if caption:
        assert_disjoint([("status caption", caption), *((name, stable[name]) for name in METADATA)])
    layout = {"dock": bounds(dock), "catalog viewport": bounds(catalog), "transport": bounds(transport),
              **{name: bounds(item) for name, item in stable.items()}}
    if known:
        check(rail and seek and seek.get("accessibleEnabled"), "known duration lost its enabled seek rail")
        layout |= {"seek rail": bounds(rail), "seek target": bounds(seek)}
    else:
        check(not rail and not seek, "unknown duration exposed a seek rail or target")
        check(not named(window, children, r"(?:Elapsed|Duration) \d+:\d{2}", "Text"),
              "unknown duration exposed dock clocks")
    width, height = window.dimensions()
    check(layout["dock"][0] == 0 and layout["dock"][2:] == (width, height)
          and abs(layout["dock"][1] - layout["catalog viewport"][3]) <= 1,
          f"dock is not anchored at the catalog edge and window bottom: {layout['dock']}")
    items = window.subtree(transport)
    controls = {}
    glyphs = {}
    for action in ACTIONS:
        button = named(window, items, action)
        check(button and button.get("accessibleEnabled") and contains(transport, button),
              f"missing, disabled or clipped transport target: {action}")
        glyph = next((e for e in items if element_type([e], "Glyph") and contains(button, e)), None)
        check(glyph, f"missing glyph: {action}")
        controls[action] = button
        glyphs[action] = glyph
        layout[f"{action} target"] = bounds(button)
        layout[f"{action} glyph"] = bounds(glyph)
    button = controls["Play|Pause"]
    check(button["accessibleLabel"] == label, f"expected {label} inside the dock")

    image = frame(window)
    if pictures:
        image.save(pictures / f"{phase}.png")
    paint = {}
    for action in ACTIONS:
        control = controls[action]
        check(has_ring(image, control) == (control["accessibleLabel"] == focus),
              f"unexpected focus paint on {control['accessibleLabel']} (expected {focus or 'no ring'})")
        paint[action] = glyph_paint(image, glyphs[action], control, action == "Play|Pause")
    if not focus:
        left, top, right, _ = bounds(button)
        x, y = round((left + right) / 2), round(top)
        check(image.getpixel((x, y + 3)) == image.getpixel((x, y + 7)),
              "primary focus separator painted after a pointer click")
    crop = tuple(map(round, layout["transport"]))
    # Keep phases fenced while sampling successive frames for transient
    # transport paint changes, not just the final settled screenshot.
    for delay in (0.04, 0.09):
        sleep(delay)
        check(not ImageChops.difference(image.crop(crop), frame(window).crop(crop)).getbbox(),
              f"transport paint flickered within {phase}")

    status_box = bounds(caption) if caption else None
    if status_box:
        ink = status_pixels(image, status_box, layout["transport"])
        check(len(ink) >= 16 and max(x for x, _ in ink) - min(x for x, _ in ink) >= 45,
              f"status did not visibly paint: {status}")
    clock_box = bounds(clock) if clock else None
    end = None
    if known:
        check(pixels(image, clock_box, lambda rgb: min(rgb) >= 145), "elapsed clock did not visibly paint")
        seek_fill = pixels(image, bounds(rail), lambda rgb: rgb == ACCENT)
        check(seek_fill, "seek progress did not visibly paint")
        end = max(x for x, _ in seek_fill)
    print(f"{phase}: dock={layout['dock']}, {label}={bounds(button)}, "
          f"ink-center={paint['Play|Pause'][1]}, seek-end={end}, status={status!r}, status-box={status_box}",
          flush=True)
    return Sample(dock["handle"], layout, paint, image, status_box, clock_box, end)


def compare(before, after, *, icon_changes=False):
    check(after.dock_handle == before.dock_handle, "playback phase replaced the dock")
    shifted = [name for name, old in before.layout.items()
               if name in after.layout and any(abs(a - b) > 0.25 for a, b in zip(old, after.layout[name]))]
    check(not shifted, "layout moved: " + ", ".join(shifted))
    if before.status and after.status:
        check(before.status == after.status, f"status lane moved: {before.status} → {after.status}")
    for action, (mask, position) in before.paint.items():
        next_mask, next_position = after.paint[action]
        if action == "Play|Pause" and icon_changes:
            check(mask != next_mask, "primary icon did not change between Play and Pause")
            check(all(abs(a - b) <= 1 for a, b in zip(position, next_position)),
                  f"primary ink center drifted on icon swap: {position} → {next_position}")
        else:
            check(mask == next_mask and position == next_position,
                  f"{action} ink moved or changed: {position} → {next_position}")


def transition(window, root, pictures, failures, width, *, keyboard=False, unknown=False, dense=False):
    options = ("-unknown-duration" if unknown else "") + ("-density-125" if dense else "")
    prefix = f"{width}-{'keyboard' if keyboard else 'pointer'}{options}"

    def read(phase, **options):
        try:
            return capture(window, f"{prefix}-{phase}", pictures, **options)
        except Failure as error:
            raise Failure(f"{prefix}-{phase}: {error}") from error

    def release(name):
        (root / name).touch(exist_ok=False)

    for gate in (("unknown-duration",) if unknown else ()) + (("dense",) if dense else ()):
        release(gate)
        wait(f"fixture consumed {gate}", lambda: not (root / gate).exists())
    if keyboard:
        # Pointer Pause from the preceding run owns real focus but no ring.
        # Tab must move to Next, not restart the focus chain elsewhere.
        window.key(TAB)
        read("tab-next", focus="Next track", known=not unknown)
        window.shortcut("\x10", TAB)  # Shift+Tab
    play_focus = "Play" if keyboard else None
    pause_focus = "Pause" if keyboard else None
    paused = read("paused", focus=play_focus, known=not unknown)
    if keyboard:
        window.key(" ")
    else:
        window.click("Play", within=player_bar(window))  # Native press/release, not an accessible action.
        move_off(window)
    request = read("request", status=REQUEST, focus=play_focus, known=not unknown)
    release("buffer")
    buffering = read("buffering", status=BUFFERING, focus=play_focus, known=not unknown)
    seek_visible = buffering
    if unknown:
        release("known-duration")
        seek_visible = read("seek-inserted", status=BUFFERING, focus=play_focus)
    release("play")
    playing = read("playing", playing=True, focus=pause_focus)
    release("tick")
    advanced = read("advanced", playing=True, ticked=True, focus=pause_focus)
    window.click("Pause", within=player_bar(window))
    move_off(window)
    restored = read("restored")

    for phase, sample in (("request", request), ("buffering", buffering), ("playing", playing),
                          ("advanced", advanced), ("restored", restored)):
        with verify(failures, f"{prefix} paused → {phase}"):
            compare(paused, sample, icon_changes=phase in ("playing", "advanced"))
    with verify(failures, f"{prefix} request → buffering"):
        compare(request, buffering)
    if unknown:
        with verify(failures, f"{prefix} seek insertion"):
            compare(buffering, seek_visible)
            crop = tuple(map(round, buffering.status))
            check(not ImageChops.difference(buffering.image.crop(crop), seek_visible.image.crop(crop)).getbbox(),
                  "buffering status paint changed when seek appeared")
            compare(seek_visible, playing, icon_changes=True)
            print(f"PASS {prefix}: seek appeared without moving the workspace, dock or buffering status", flush=True)
    with verify(failures, f"{prefix} visible playback changes"):
        for sample in (request, seek_visible):
            old_ink = status_pixels(sample.image, sample.status, sample.layout["transport"])
            remaining = sum(status_ink(playing.image.getpixel(point)) for point in old_ink)
            check(remaining < len(old_ink) / 10, "status ink remained after playback started")
        check(advanced.seek_end > playing.seek_end + 15, "seek fill did not advance visibly during playback")
        before, after = (sample.image.crop(tuple(map(round, sample.clock))) for sample in (playing, advanced))
        check(ImageChops.difference(before, after).getbbox(), "elapsed clock pixels did not change")
        print(f"PASS {prefix}: status cleared, elapsed clock and seek fill advanced", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("example_binary", type=Path, help="prebuilt dock_transport_acceptance example")
    parser.add_argument("--pixels", type=Path, help="new directory for disposable phase screenshots")
    args = parser.parse_args()
    if args.pixels:
        args.pixels.mkdir()
    failures = []
    with TemporaryDirectory(prefix="spofify-dock-transport-") as name:
        root = Path(name)
        binary = root / "dock-transport-acceptance"
        shutil.copy2(args.example_binary.resolve(strict=True), binary)
        with desktop(binary, root, private_env(root), multisize=True) as window:
            for mode in (MINIMUM, COMPACT, WIDE):
                window.resize(mode)
                transition(window, root, args.pixels, failures, mode[0])
            window.resize(COMPACT)
            transition(window, root, args.pixels, failures, COMPACT[0], keyboard=True, unknown=True)
            window.resize(MINIMUM)
            transition(window, root, args.pixels, failures, MINIMUM[0], dense=True)
    check(not failures, f"{len(failures)} dock transition check(s) failed")
    print("PASS dock and painted transport stay stable across pointer and keyboard playback transitions")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        sys.exit(f"FAIL: {error}")
