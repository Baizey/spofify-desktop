#!/usr/bin/env python3
"""Exercise the dock's native output picker without account, audio, or an installed app.

Build in a private target directory with SLINT_EMIT_DEBUG_INFO=1 and
`cargo build --locked --features slint/mcp --example playback_acceptance`.
Run this script with that directory's debug/examples/playback_acceptance binary.
"""
import argparse
from pathlib import Path
import re
import shutil
import sys
from tempfile import TemporaryDirectory

sys.dont_write_bytecode = True
from fixture import check, private_env
from native import ESCAPE, RETURN, bounds, contains, desktop, element_id, named, wait


MINIMUM, WIDE = (480, 440), (1280, 790)  # Client pixels, excluding Cage decoration.
BUTTON = ("Button",)
TEXT = ("Text",)
TRIGGER = "Playback devices"
CLOSE = "Close playback devices"
MANAGE = "Manage devices"
DESKTOP_OUTPUT = ("desktop-1", "Desktop speakers")
STUDIO_OUTPUT = ("remote-7", "Studio receiver")
TAB = "\t"


def option(name):
    return "Select output: " + re.escape(name)


def resize(window, size):
    width, height = size
    if window.dimensions() != size:
        # Probe above the minimum to measure compositor decoration, as in settings.py.
        trial = height + 64
        actual = window.resize((width, trial))["height"]
        if actual != height:
            window.resize((width, trial + height - actual))
    check(window.dimensions() == size, f"expected exact picker client {size}, got {window.dimensions()}")


def open_panel(window):
    window.find(TRIGGER, roles=TEXT)
    panel = element_id(window.elements(), "panel")
    check(panel, "missing the open dock picker panel in Slint's native element tree")
    return panel


def visible_button(window, region, label):
    button = named(window, window.elements(), label)
    return button if button and contains(region, button) else None


def closed(window):
    window.absent(CLOSE, roles=BUTTON)
    window.absent(TRIGGER, roles=TEXT)


def trigger(window, output):
    control = window.find(TRIGGER, roles=BUTTON)
    dock = element_id(window.elements(), "player")
    check(dock and contains(dock, control), "Playback devices is not a dock control")
    check(control.get("accessibleEnabled") and control.get("accessibleDescription") == output,
          f"dock trigger did not expose current output {output!r}: {control.get('accessibleDescription')!r}")
    return control


def selected_row(window, name):
    selected = window.find(re.escape(name), roles=TEXT)
    check(selected.get("accessibleDescription") == "Selected output", "current output lacks its selected state")
    window.absent(option(name), roles=BUTTON)
    return selected


def no_deselect(window):
    window.absent(r"Deselect(?: output)?|No playback device", roles=BUTTON)


def history(window):
    window.section("History")
    window.absent("Back", roles=BUTTON)
    window.absent("Devices", roles=TEXT)
    window.find("Now playing: Synthetic Tone 001", roles=BUTTON)


def close_and_measure(window, panel, switch):
    close = window.find(CLOSE, roles=BUTTON)
    check(contains(panel, close) and contains(panel, window.find(MANAGE, roles=BUTTON)),
          "popup clips its Close or Manage devices control")
    left, top, right, bottom = bounds(panel)
    cx, cy, cright, cbottom = bounds(close)
    # Slint's element coordinates are popup-local. The press recording at the
    # center of Close supplies the actual translation into the client window.
    x, y = window.pointer_position_for_click(CLOSE, element=close)
    dx, dy = x - (cx + cright) / 2, y - (cy + cbottom) / 2
    left, top, right, bottom = left + dx, top + dy, right + dx, bottom + dy
    sx, sy, sright, _ = bounds(switch)
    width, _ = window.dimensions()
    check(310 <= right - left <= 342 and 125 <= bottom - top <= 286.5,
          f"unexpected popup size: {right - left:g} × {bottom - top:g}")
    check(0 <= left <= sx < sright <= right <= width and 0 <= top < bottom <= sy <= bottom + 18,
          f"picker {(left, top, right, bottom)} is not visibly anchored above trigger {bounds(switch)}")
    closed(window)
    print(f"PASS {width:g}px popup geometry: {right - left:g} × {bottom - top:g}, anchored above dock trigger",
          flush=True)


def check_size(window, mode, before, after, activate, expect):
    _, old_name = before
    new_id, new_name = after
    switch = trigger(window, old_name)
    window.click_element(switch)
    panel = open_panel(window)
    selected = selected_row(window, old_name)
    alternative = window.find(option(new_name), roles=BUTTON)
    check(alternative.get("accessibleEnabled"), f"eligible output {new_name!r} is disabled")
    check(bounds(selected)[1] < bounds(alternative)[1], "current output is not shown before alternatives")
    no_deselect(window)
    window.absent("Rename device", roles=BUTTON)
    window.click_element(selected)
    for name, reason in (("Travel speaker", "Not available"), ("Library browser", "Cannot play audio")):
        disabled = window.find(option(name), roles=BUTTON)
        check(not disabled.get("accessibleEnabled") and disabled.get("accessibleDescription") == reason,
              f"{name!r} is actionable or missing its accessibility reason")
        window.click_element(disabled)
    open_panel(window)  # Clicking passive or disabled rows must not dismiss or act.
    expect()
    window.pixels(f"dock-device-picker-{mode[0]}", mode[0])
    close_and_measure(window, panel, switch)
    history(window)

    # No deliberate refocus: every dismiss mechanism must return to the trigger.
    window.key(RETURN)
    open_panel(window)
    window.key(ESCAPE)
    closed(window)
    window.key(RETURN)
    open_panel(window)
    # The top-left rail brand is inert and outside the dock popup; the broad
    # catalog heading can have its center underneath the popup at 480px.
    window.click_element(window.title("Spofify"))
    closed(window)
    history(window)
    window.key(RETURN)
    open_panel(window)
    if activate == "keyboard":
        window.key(TAB)
        window.key(RETURN)
    else:
        window.click(option(new_name))
    closed(window)
    trigger(window, new_name)  # Wait for the synthetic model acknowledgement.
    history(window)
    expect(("device", "select", new_id))
    window.key(RETURN)
    open_panel(window)
    selected_row(window, new_name)
    check(window.find(option(old_name), roles=BUTTON).get("accessibleEnabled"),
          "previous output did not become an enabled alternative")
    window.click(MANAGE)
    window.title("Devices")
    closed(window)
    expect(("navigate", "devices"))
    no_deselect(window)
    catalog = element_id(window.elements(), "catalog-scroll")
    check(catalog, "missing native Devices catalog viewport")
    # MCP omits a fully clipped row, so seek the selected card by scrolling
    # the viewport, not by looking up the offscreen control's position.
    for _ in range(15):
        if visible_button(window, catalog, "Rename device"):
            break
        window.mcp.call("scroll_element", {"elementHandle": catalog["handle"], "deltaY": -95})
    rename = visible_button(window, catalog, "Rename device")
    check(rename and rename.get("accessibleEnabled"), "enabled Rename device not revealed in Devices")
    no_deselect(window)
    window.click_element(rename)
    window.title("Rename playback device")
    field = window.find("Device name", roles=("TextInput",))
    check(field.get("accessibleValue") == new_name, "Manage devices opened Rename for the wrong output")
    expect(("device", "prepare-rename", new_id))
    window.click("Cancel")
    window.absent("Rename playback device", roles=TEXT)
    catalog = element_id(window.elements(), "catalog-scroll")
    window.mcp.call("scroll_element", {"elementHandle": catalog["handle"], "deltaY": 2000})
    window.title("Devices")
    window.click("Back")
    history(window)
    expect(("navigate", "back"))
    print(f"PASS {mode[0]}px dismiss/focus return, {activate} selection without navigation, and explicit management",
          flush=True)


def roster_viewport(window):
    panel = open_panel(window)
    viewport = element_id(window.subtree(panel, max_elements=1000), "scroll")
    check(viewport, "missing bounded picker roster viewport")
    return viewport


def long_roster(window, expect):
    window.click(TRIGGER)
    viewport = roster_viewport(window)
    last_label = option("Room output 08")
    window.absent(last_label, roles=BUTTON)
    pinned = {label: bounds(window.find(label, roles=BUTTON)) for label in (CLOSE, MANAGE)}
    window.mcp.call("scroll_element", {"elementHandle": viewport["handle"], "deltaY": -1000})
    wait("last output revealed by wheel", lambda: visible_button(window, viewport, last_label))
    check(all(bounds(window.find(label, roles=BUTTON)) == pos for label, pos in pinned.items()),
          "wheel scrolled the picker header or management footer")
    window.key(ESCAPE)
    closed(window)
    window.key(RETURN)
    viewport = roster_viewport(window)
    window.absent(last_label, roles=BUTTON)
    # One first alternative, then eight room outputs. Neither of the two
    # disabled alternatives nor the selected row may consume a Tab stop.
    for _ in range(9):
        window.key(TAB)
    wait("last output revealed by Tab", lambda: visible_button(window, viewport, last_label))
    window.key(RETURN)
    closed(window)
    history(window)
    trigger(window, "Room output 08")
    expect(("device", "select", "extra-8"))
    print("PASS long roster wheel and Tab reveal a clipped choice; header and management stay pinned", flush=True)


def live_resize(window, expect):
    output = "Room output 08"
    for size in (MINIMUM, WIDE):
        window.click_element(trigger(window, output))
        open_panel(window)
        resize(window, size)
        # No click to reopen: the existing popup must still be visible and must
        # have recomputed its native pointer offset against the rebuilt dock.
        panel = open_panel(window)
        selected_row(window, output)
        close_and_measure(window, panel, trigger(window, output))
        history(window)
        expect()
    print("PASS same open picker stays anchored while the native window resizes in both directions", flush=True)


def lifecycle(window, journal, expect):
    output = "Room output 08"
    control = journal.with_suffix(".control")
    pending = control.with_suffix(".tmp")

    def update(command):
        # Post a complete command; the fixture removes it after applying state.
        pending.write_text(command)
        pending.replace(control)
        wait(f"fixture state {command}", lambda: not control.exists())

    window.click_element(trigger(window, output))
    open_panel(window)
    update("change-page-key")
    closed(window)
    history(window)
    trigger(window, output)
    expect()

    window.click(TRIGGER)
    open_panel(window)
    update("drop-dock")
    closed(window)
    check(not element_id(window.elements(), "player"), "fixture did not remove the dock")
    expect()
    update("restore-dock")
    history(window)
    closed(window)
    window.click_element(trigger(window, output))
    open_panel(window)
    window.key(ESCAPE)
    closed(window)
    expect()
    print("PASS external route-context change and dock removal close the picker without callbacks", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("example_binary", type=Path, help="prebuilt disposable playback_acceptance example")
    parser.add_argument("--screenshots", type=Path, help="new private directory for the two native PNGs")
    args = parser.parse_args()
    if args.screenshots:
        args.screenshots.mkdir(mode=0o700)
    with TemporaryDirectory(prefix="spofify-picker-") as name:
        root = Path(name)
        binary = root / "playback-acceptance"
        shutil.copy2(args.example_binary.resolve(strict=True), binary)
        journal = root / "picker-events.tsv"
        env = dict(private_env(root), SPOFIFY_PICKER_FIXTURE_JOURNAL=str(journal))
        expected = []

        def expect(*events):
            expected.extend(events)
            actual = [tuple(line.split("\t")) for line in journal.read_text().splitlines()]
            check(actual == expected, f"unexpected fixture callbacks: {actual}; expected {expected}")

        with desktop(binary, root, env, args.screenshots, multisize=True) as window:
            for mode, before, after, activate in ((MINIMUM, DESKTOP_OUTPUT, STUDIO_OUTPUT, "keyboard"),
                                                  (WIDE, STUDIO_OUTPUT, DESKTOP_OUTPUT, "pointer")):
                resize(window, mode)
                check_size(window, mode, before, after, activate, expect)
            long_roster(window, expect)
            live_resize(window, expect)
            lifecycle(window, journal, expect)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        sys.exit(f"FAIL: {error}")
