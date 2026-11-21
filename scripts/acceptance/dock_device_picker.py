#!/usr/bin/env python3
"""Exercise the native playback devices popup without an installed app or account.

Build privately with SLINT_EMIT_DEBUG_INFO=1 and
`cargo build --locked --features slint/mcp --example playback_acceptance`.
Pass the resulting debug/examples/playback_acceptance binary.
"""
import argparse
from pathlib import Path
import re
import shutil
import sys
from tempfile import TemporaryDirectory

sys.dont_write_bytecode = True
from fixture import check, private_env
from native import CONTROL, ESCAPE, RETURN, bounds, contains, desktop, element_id, named, wait


MINIMUM, WIDE = (480, 440), (1280, 790)  # Client pixels, excluding Cage decoration.
BUTTON = ("Button",)
TEXT = ("Text",)
TRIGGER = "Playback devices"
CLOSE = "Close playback devices"
DESKTOP_OUTPUT = ("desktop-1", "Desktop speakers")
STUDIO_OUTPUT = ("remote-7", "Studio receiver")
TAB = "\t"


def option(name):
    return "Select output: " + re.escape(name)


def rename(name):
    return "Rename device: " + re.escape(name)


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
    check(panel, "missing the open picker panel in Slint's native element tree")
    window.absent(r"Manage devices|Deselect(?: output)?|No playback device", roles=BUTTON)
    return panel


def closed(window):
    window.absent(CLOSE, roles=BUTTON)
    window.absent(TRIGGER, roles=TEXT)
    window.absent("Device name", roles=("TextInput",))


def trigger(window, entry, output=None):
    control = window.find(TRIGGER, roles=BUTTON)
    dock = element_id(window.elements(), "player")
    region = dock if entry == "dock" else element_id(window.elements(), "toolbar")
    check((entry == "dock") == bool(dock), f"incorrect dock presence in {entry}")
    check(region and contains(region, control), f"Playback devices is not a {entry} control")
    check(control.get("accessibleEnabled"), f"disabled {entry} trigger")
    if output:
        check(control.get("accessibleDescription") == output,
              f"{entry} trigger did not announce {output!r}: {control.get('accessibleDescription')!r}")
    return control


def selected_row(window, name):
    selected = window.find(re.escape(name), roles=TEXT)
    check(selected.get("accessibleDescription") == "Selected output", "current output lacks its selected state")
    window.absent(option(name), roles=BUTTON)
    return selected


def assert_view(window, entry):
    window.absent("Devices", roles=TEXT)
    window.absent("Rename playback device", roles=TEXT)
    if entry == "now":
        window.find("Back", roles=BUTTON)
        window.title("Synthetic Tone 001")
        window.find("Elapsed 0:46", roles=TEXT)
        window.find("Duration 3:13", roles=TEXT)
    else:
        window.section("History")
        window.absent("Back", roles=BUTTON)
        if entry == "dock":
            window.find("Now playing: Synthetic Tone 001", roles=BUTTON)
        else:
            window.absent(r"Now playing: .+", roles=BUTTON)


def queue_state(window):
    return (bool(element_id(window.elements(), "queue-pane")),
            window.find("Play queue", roles=BUTTON).get("accessibleExpanded", False))


def close_and_measure(window, panel, switch, entry):
    close = window.find(CLOSE, roles=BUTTON)
    check(contains(panel, close), "popup clips its Close control")
    left, top, right, bottom = bounds(panel)
    cx, cy, cright, cbottom = bounds(close)
    # Slint's element coordinates are popup-local. Record where the native click landed.
    x, y = window.pointer_position_for_click(CLOSE, element=close)
    dx, dy = x - (cx + cright) / 2, y - (cy + cbottom) / 2
    left, top, right, bottom = left + dx, top + dy, right + dx, bottom + dy
    sx, sy, sright, sbottom = bounds(switch)
    width, height = window.dimensions()
    check(310 <= right - left <= 342 and 50 <= bottom - top <= 286.5,
          f"unexpected popup size: {right - left:g} × {bottom - top:g}")
    check(0 <= left <= sx < sright <= right <= width and 0 <= top < bottom <= height,
          f"picker {(left, top, right, bottom)} clips or misses trigger {bounds(switch)}")
    if entry == "dock":
        check(0 <= sy - bottom <= 18, f"popup is not above the dock trigger: {sy - bottom:g}px")
    else:
        check(0 <= top - sbottom <= 18, f"popup is not below the toolbar trigger: {top - sbottom:g}px")
        check(abs(right - min(width - 8, max(sright, right - left + 8))) <= 1,
              f"popup right edge {right:g}px is not aligned to toolbar trigger {sright:g}px")
    closed(window)
    print(f"PASS {width:g}px {entry} popup geometry: {right - left:g} × {bottom - top:g}", flush=True)


def select_journey(window, width, entry, before, after, keyboard, expect):
    _, old_name = before
    new_id, new_name = after
    assert_view(window, entry)
    queue = queue_state(window)
    switch = trigger(window, entry, old_name)
    window.click_element(switch)
    panel = open_panel(window)
    selected = selected_row(window, old_name)
    alternative = window.find(option(new_name), roles=BUTTON)
    check(alternative.get("accessibleEnabled") and bounds(selected)[1] < bounds(alternative)[1],
          "selected output must be passive and above the next available output")
    pencil = window.find(rename(old_name), roles=BUTTON)
    check(abs(bounds(pencil)[1] - bounds(selected)[1]) < 10, "Rename must be next to the selected device")
    window.click_element(selected)
    for name, reason in (("Travel speaker", "Not available"), ("Library browser", "Cannot play audio")):
        disabled = window.find(option(name), roles=BUTTON)
        check(not disabled.get("accessibleEnabled") and disabled.get("accessibleDescription") == reason,
              f"{name!r} is actionable or missing its accessibility reason")
        check(bounds(alternative)[1] < bounds(disabled)[1], "disabled output is above usable choices")
        window.click_element(disabled)
    open_panel(window)
    expect()
    window.pixels(f"devices-{entry}-{width}", width)
    close_and_measure(window, panel, switch, entry)

    # Enter reopens the action without manually moving focus after each dismiss.
    window.key(RETURN)
    open_panel(window)
    window.key(ESCAPE)
    closed(window)
    window.key(RETURN)
    open_panel(window)
    window.click_element(window.title("Spofify"))  # Inert rail brand outside both anchors.
    closed(window)
    window.key(RETURN)
    open_panel(window)
    if keyboard:
        window.key(TAB)  # First stop is the eligible alternative, not the pencil.
        window.key(RETURN)
    else:
        window.click(option(new_name))
    closed(window)
    trigger(window, entry, new_name)
    assert_view(window, entry)
    check(queue_state(window) == queue, "device selection changed the queue state")
    expect(("device", "select", new_id))
    window.key(RETURN)
    open_panel(window)
    selected_row(window, new_name)
    window.key(ESCAPE)
    closed(window)
    print(f"PASS {width}px {entry}: {'keyboard' if keyboard else 'pointer'} selection, dismiss and focus return, no navigation", flush=True)


def rename_journey(window, width, entry, output, new_name, keyboard, expect):
    ident, old_name = output
    window.click_element(trigger(window, entry, old_name))
    open_panel(window)
    if keyboard:
        window.key(TAB)  # The sole eligible alternative precedes Rename.
        window.key(TAB)
        window.key(RETURN)
    else:
        window.click(rename(old_name))
    field = window.find("Device name", roles=("TextInput",))
    check(field.get("accessibleValue") == old_name, "Rename did not prefill the selected device")
    window.shortcut(CONTROL, "a")  # Must already be focused; no clicking into the field.
    window.key("Discard this")
    check(window.find("Device name", roles=("TextInput",)).get("accessibleValue") == "Discard this", "rename input did not receive focus")
    window.key(ESCAPE)
    open_panel(window)
    window.absent("Device name", roles=("TextInput",))
    selected_row(window, old_name)
    expect()
    window.key(RETURN)  # Escape in the editor returns to the pencil, not the trigger.
    check(window.find("Device name", roles=("TextInput",)).get("accessibleValue") == old_name,
          "rename cancellation retained a stale draft or did not restore focus")
    window.shortcut(CONTROL, "a")
    window.key(new_name)
    window.pixels(f"devices-rename-{entry}-{width}", width)
    if keyboard:
        window.key(RETURN)
    else:
        window.click("Save device name")
    expect(("device", "rename", ident, new_name))
    open_panel(window)
    selected_row(window, new_name)
    window.key(ESCAPE)
    closed(window)
    assert_view(window, entry)
    trigger(window, entry, new_name)
    print(f"PASS {width}px {entry}: inline rename, Escape reverts draft, exact device ID/name on save", flush=True)


def roster_viewport(window):
    panel = open_panel(window)
    viewport = element_id(window.subtree(panel, max_elements=1000), "scroll")
    check(viewport, "missing bounded picker roster viewport")
    return viewport


def visible_button(window, region, label):
    button = named(window, window.elements(), label)
    return button if button and contains(region, button) else None


def long_roster(window, entry, expect):
    window.click(TRIGGER)
    viewport = roster_viewport(window)
    last_label = option("Room output 08")
    window.absent(last_label, roles=BUTTON)
    window.absent(option("Travel speaker"), roles=BUTTON)
    window.find(option("Room output 02"), roles=BUTTON)
    fixed = (bounds(window.find(CLOSE, roles=BUTTON)), bounds(selected_row(window, DESKTOP_OUTPUT[1])))
    window.mcp.call("scroll_element", {"elementHandle": viewport["handle"], "deltaY": -1000})
    wait("last choice revealed by wheel", lambda: visible_button(window, viewport, last_label))
    check(fixed == (bounds(window.find(CLOSE, roles=BUTTON)), bounds(selected_row(window, DESKTOP_OUTPUT[1]))),
          "wheel scrolled the title or selected output out of place")
    window.find(option("Travel speaker"), roles=BUTTON)
    window.key(ESCAPE)
    closed(window)
    window.key(RETURN)
    viewport = roster_viewport(window)
    for _ in range(9):  # One first alternative, then eight room outputs (before Rename).
        window.key(TAB)
    wait("last choice visible on Tab focus", lambda: visible_button(window, viewport, last_label))
    window.key(RETURN)
    closed(window)
    assert_view(window, entry)
    trigger(window, entry, "Room output 08")
    expect(("device", "select", "extra-8"))
    print(f"PASS {entry} long roster: usable choices first, wheel and Tab scroll, current output stays visible", flush=True)


def live_resize(window, entry, expect):
    # Resize while open (including a layout breakpoint); do not reopen to inspect the new anchor.
    for size in (MINIMUM, WIDE):
        window.click_element(trigger(window, entry, "Room output 08"))
        open_panel(window)
        resize(window, size)
        panel = open_panel(window)
        selected_row(window, "Room output 08")
        close_and_measure(window, panel, trigger(window, entry, "Room output 08"), entry)
        assert_view(window, entry)
        expect()
    print(f"PASS {entry} stays anchored while open through window resize", flush=True)


def lifecycle(window, update, expect):
    update("history")
    update("restore-dock")
    window.click_element(trigger(window, "dock"))
    open_panel(window)
    update("change-page-key")
    closed(window)
    window.key(RETURN)
    open_panel(window)
    update("drop-dock")
    closed(window)
    window.click_element(trigger(window, "toolbar"))
    open_panel(window)
    update("restore-dock")
    closed(window)
    trigger(window, "dock")
    update("now")
    window.click_element(trigger(window, "now"))
    open_panel(window)
    update("history")
    closed(window)
    assert_view(window, "dock")
    expect()
    print("PASS external route changes and either trigger disappearing close the popup without callbacks", flush=True)


def roster_states(window, update, expect):
    update("drop-dock")
    for state, max_height in (("empty", 86), ("one", 94)):
        update(state)
        window.click(TRIGGER)
        panel = open_panel(window)
        _, top, _, bottom = bounds(panel)
        check(50 <= bottom - top <= max_height, f"{state} popup isn't compact: {bottom - top:g}px")
        if state == "empty":
            window.title("No outputs available")
            window.absent(r"Rename device: .+|Select output: .+", roles=BUTTON)
        else:
            selected_row(window, DESKTOP_OUTPUT[1])
            window.find(rename(DESKTOP_OUTPUT[1]), roles=BUTTON)
            window.absent(r"Select output: .+", roles=BUTTON)
        window.key(ESCAPE)
        closed(window)
    update("mixed")
    for mode, message in (("offline", "Device control is unavailable offline"), ("reconnect", "Connecting…"), ("disconnect", None)):
        update(mode)
        window.click(TRIGGER)
        open_panel(window)
        if message:
            window.title(message)
            selected = window.title(DESKTOP_OUTPUT[1])  # stale roster is deliberately supplied by the fixture
            check(not (selected.get("accessibleDescription") or "").startswith("Selected output"), "fake offline selection")
        else:
            trigger(window, "toolbar", "Output disconnected")
            window.absent(re.escape(DESKTOP_OUTPUT[1]), roles=TEXT)
        window.absent(r"Rename device: .+", roles=BUTTON)
        alternate = window.find(option(STUDIO_OUTPUT[1]), roles=BUTTON)
        check(not alternate.get("accessibleEnabled"), f"{mode} lets user select an output")
        window.click_element(alternate)
        open_panel(window)
        window.key(ESCAPE)
        closed(window)
        update("online")
    expect()
    print("PASS empty/one stay small; offline, reconnect and disconnect never show a false selection or write action", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("example_binary", type=Path, help="prebuilt disposable playback_acceptance example")
    parser.add_argument("--screenshots", type=Path, help="new private directory for native PNGs")
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

        def update(command):
            control = journal.with_suffix(".control")
            pending = control.with_suffix(".tmp")
            pending.write_text(command)
            pending.replace(control)
            wait(f"fixture state {command}", lambda: not control.exists())

        with desktop(binary, root, env, args.screenshots, multisize=True) as window:
            for size, keyboard in ((MINIMUM, True), (WIDE, False)):
                resize(window, size)
                for entry in ("dock", "toolbar", "now"):
                    update("history")
                    update("drop-dock" if entry == "toolbar" else "restore-dock")
                    if entry == "now":
                        update("now")
                    update("mixed")
                    select_journey(window, size[0], entry, DESKTOP_OUTPUT, STUDIO_OUTPUT, keyboard, expect)
                    if entry in ("dock", "toolbar"):
                        rename_journey(window, size[0], entry, STUDIO_OUTPUT, f"Studio {entry} {size[0]}", keyboard, expect)
            update("history")
            for entry in ("dock", "toolbar"):
                update("restore-dock" if entry == "dock" else "drop-dock")
                update("long")
                long_roster(window, entry, expect)
                live_resize(window, entry, expect)
            lifecycle(window, update, expect)
            resize(window, MINIMUM)
            roster_states(window, update, expect)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        sys.exit(f"FAIL: {error}")
