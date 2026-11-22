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
from native import (CONTROL, ESCAPE, RETURN, assert_disjoint, assert_empty_dock, bounds, contains,
                    desktop, dock_identity, element_id, element_type, named, player_bar, wait)


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


def minimum_rail_settings(window, update, expect, density):
    check(window.dimensions() == MINIMUM, "rail Settings check requires the minimum client size")
    elements = window.elements()
    dock = dock_identity(window, elements)
    rail = element_id(elements, "rail")
    check(rail, "missing the navigation rail")
    settings = window.find("Settings", roles=BUTTON, within=rail, timeout=3)
    check(contains(rail, settings, slack=0) and bounds(settings)[3] <= dock[1][1],
          f"{density}%: Settings {bounds(settings)} is clipped by rail {bounds(rail)} or dock {dock[1]}")
    window.click_element(settings)
    window.section("Settings")
    expect(("navigate", "settings"))
    check(dock_identity(window) == dock, "opening Settings at minimum size moved or replaced the dock")
    update("history")
    print(f"PASS 480×440 at {density}%: Settings fits above the dock and opens by pointer", flush=True)


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


def trigger(window, output=None):
    elements = window.elements()
    dock = player_bar(window, elements)
    controls = [e for e in elements if window.matches(e, TRIGGER) and e.get("accessibleRole") == "Button"]
    check(len(controls) == 1, f"expected one Playback devices trigger, found {len(controls)}")
    control = controls[0]
    check(control.get("accessibleEnabled") and contains(dock, control), "device trigger is disabled or outside the dock")
    if output:
        check(control.get("accessibleDescription") == output,
              f"dock trigger did not announce {output!r}: {control.get('accessibleDescription')!r}")
    return control


def selected_row(window, name):
    selected = window.find(re.escape(name), roles=TEXT)
    check(selected.get("accessibleDescription") == "Selected output", "current output lacks its selected state")
    window.absent(option(name), roles=BUTTON)
    return selected


def center_y(element):
    _, top, _, bottom = bounds(element)
    return (top + bottom) / 2


def assert_popup_guides(window, panel, *, stacked_editor=False):
    elements = window.subtree(panel)
    title = named(window, elements, TRIGGER, "Text")
    close = named(window, elements, CLOSE)
    header = element_id(elements, "header")
    guide = bounds(title)[0]
    check(abs(guide - bounds(panel)[0] - 32) <= 1, f"popup text guide drifted: {guide:g}px")
    check(abs(center_y(title) - center_y(header)) <= 1 and abs(center_y(close) - center_y(header)) <= 1,
          "popup heading and Close are not centered in the header")

    captions = [e for e in elements if any(t.get("typeName") == "Caption" for t in e.get("typeNamesAndIds", []))]
    choices = [e for e in elements if (e.get("accessibleLabel") or "").startswith("Select output: ")]
    labels = [(e.get("accessibleLabel"), e) for e in captions]
    for choice in choices:
        words = element_id(window.subtree(choice), "words")
        check(words and contains(choice, words), f"device name escapes its list row: {choice['accessibleLabel']}")
        labels.append((choice["accessibleLabel"], words))
    selected = element_id(elements, "selected-name")
    if selected:
        labels.append((selected["accessibleLabel"], selected))
    field = named(window, elements, "Device name", "TextInput")
    if field:
        labels.append(("rename input text", field))
    for label, element in labels:
        check(abs(bounds(element)[0] - guide) <= 1, f"{label!r} is not on the popup text guide: {bounds(element)}")

    pencil = element_id(elements, "pencil")
    mark = element_id(elements, "checkmark")
    if pencil:
        check(mark and selected and contains(panel, mark), "current output lacks a separate check gutter")
        check(bounds(mark)[2] + 3 <= guide and abs(center_y(mark) - center_y(selected)) <= 1,
              "checkmark is not in the selected-name gutter")
        check(abs(center_y(pencil) - center_y(selected)) <= 1 and abs(bounds(pencil)[0] - bounds(close)[0]) <= 1
              and abs(bounds(pencil)[2] - bounds(close)[2]) <= 1, "Rename is off the right action/selected row guides")
        check(bounds(selected)[2] + 4 <= bounds(pencil)[0], "selected name touches or runs beneath Rename")
    else:
        check(not mark, "unexpected checkmark without a current row")
    if field:
        lane = element_id(elements, "selected-row")
        field_box = element_id(elements, "name-field")
        save, cancel = (named(window, elements, name) for name in ("Save device name", "Cancel device rename"))
        controls = (("Rename field", field_box), ("Save", save), ("Cancel", cancel))
        for label, control in controls:
            check(control and contains(lane, control), f"{label} does not fit the pinned rename editor")
        check(abs(center_y(save) - center_y(cancel)) <= 1, "rename actions do not share a centerline")
        check(abs(bounds(cancel)[2] - bounds(close)[2]) <= 1, "rename editor misses the right action guide")
        if stacked_editor:
            check(bounds(field_box)[3] < bounds(save)[1], "enlarged rename controls are not below the field")
            check(abs(bounds(field_box)[2] - bounds(cancel)[2]) <= 1, "enlarged rename field is not full width")
        else:
            check(all(abs(center_y(control) - center_y(lane)) <= 1 for _, control in controls),
                  "rename editor controls are not on the selected row centerline")
        assert_disjoint(controls)


def assert_view(window, entry):
    window.absent("Devices", roles=TEXT)
    window.absent("Rename playback device", roles=TEXT)
    if entry == "now":
        window.find("Back", roles=BUTTON)
        page = wait("Now Playing", lambda: element_type(window.elements(), "NowPlaying"))
        window.scroll_catalog(1000, within=page)
        window.find("Synthetic Tone 001", roles=TEXT, within=page)
        times = ("Elapsed 0:46", "Duration 3:13")
        elements = window.subtree(page, max_elements=1000)
        if not all((clock := named(window, elements, label, "Text")) and contains(page, clock) for label in times):
            window.scroll_catalog(-100, within=page)  # Minimum-height client can clip the page clocks.
        for label in times:
            check(contains(page, window.find(label, roles=TEXT, within=page)), f"Now Playing clips {label}")
    else:
        window.section("History")
        window.absent("Back", roles=BUTTON)
        if entry == "empty":
            assert_empty_dock(window)
        else:
            dock = player_bar(window)
            window.find("Open Now Playing", roles=BUTTON, within=dock)
            window.find("(Open album for )?Synthetic Tone 001", roles=("Button", "Text"), within=dock)


def queue_state(window):
    return (bool(element_id(window.elements(), "queue-pane")),
            window.find("Play queue", roles=BUTTON, within=player_bar(window)).get("accessibleExpanded", False))


def close_and_measure(window, panel, switch, entry):
    close = window.find(CLOSE, roles=BUTTON)
    check(contains(panel, close), "popup clips its Close control")
    left, top, right, bottom = bounds(panel)
    cx, cy, cright, cbottom = bounds(close)
    # Slint's element coordinates are popup-local. Record where the native click landed.
    x, y = window.pointer_position_for_click(CLOSE, element=close)
    dx, dy = x - (cx + cright) / 2, y - (cy + cbottom) / 2
    left, top, right, bottom = left + dx, top + dy, right + dx, bottom + dy
    sx, sy, sright, _ = bounds(switch)
    width, height = window.dimensions()
    check(310 <= right - left <= 342 and 50 <= bottom - top <= 286.5,
          f"unexpected popup size: {right - left:g} × {bottom - top:g}")
    check(0 <= left <= sx < sright <= right <= width and 0 <= top < bottom <= height,
          f"picker {(left, top, right, bottom)} clips or misses trigger {bounds(switch)}")
    check(0 <= sy - bottom <= 18, f"popup is not above the dock trigger: {sy - bottom:g}px")
    closed(window)
    print(f"PASS {width:g}px {entry} popup geometry: {right - left:g} × {bottom - top:g}", flush=True)


def select_journey(window, width, entry, before, after, keyboard, expect):
    _, old_name = before
    new_id, new_name = after
    assert_view(window, entry)
    queue = queue_state(window)
    dock = dock_identity(window)
    switch = trigger(window, old_name)
    window.click_element(switch)
    panel = open_panel(window)
    assert_popup_guides(window, panel)
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
    window.click_element(window.title("Spofify"))  # Inert rail brand outside the popup.
    closed(window)
    window.key(RETURN)
    open_panel(window)
    if keyboard:
        window.key(TAB)  # First stop is the eligible alternative, not the pencil.
        window.key(RETURN)
    else:
        window.click(option(new_name))
    closed(window)
    trigger(window, new_name)
    check(dock_identity(window) == dock, "selecting an output moved or replaced the dock")
    assert_view(window, entry)
    check(queue_state(window) == queue, "device selection changed the queue state")
    expect(("device", "select", new_id))
    window.key(RETURN)
    assert_popup_guides(window, open_panel(window))
    selected_row(window, new_name)
    window.key(ESCAPE)
    closed(window)
    print(f"PASS {width}px {entry}: {'keyboard' if keyboard else 'pointer'} selection, dismiss and focus return, no navigation", flush=True)


def rename_journey(window, width, entry, output, new_name, keyboard, expect):
    ident, old_name = output
    window.click_element(trigger(window, old_name))
    open_panel(window)
    if keyboard:
        window.key(TAB)  # The sole eligible alternative precedes Rename.
        window.key(TAB)
        window.key(RETURN)
    else:
        window.click(rename(old_name))
    field = window.find("Device name", roles=("TextInput",))
    assert_popup_guides(window, open_panel(window))
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
    trigger(window, new_name)
    print(f"PASS {width}px {entry}: inline rename, Escape reverts draft, exact device ID/name on save", flush=True)


def roster_viewport(window):
    panel = open_panel(window)
    viewport = element_id(window.subtree(panel, max_elements=1000), "scroll")
    check(viewport, "missing bounded picker roster viewport")
    return viewport


def visible_button(window, region, label):
    button = named(window, window.elements(), label)
    return button if button and contains(region, button) else None


def long_roster(window, expect):
    window.click_element(trigger(window))
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
    assert_view(window, "empty")
    trigger(window, "Room output 08")
    expect(("device", "select", "extra-8"))
    print("PASS empty-dock long roster: usable choices first, wheel and Tab scroll, current output stays visible", flush=True)


def live_resize(window, expect):
    # Resize while open (including a layout breakpoint); do not reopen to inspect the new anchor.
    handle, _ = dock_identity(window)
    for size in (MINIMUM, WIDE):
        window.click_element(trigger(window, "Room output 08"))
        open_panel(window)
        resize(window, size)
        panel = open_panel(window)
        selected_row(window, "Room output 08")
        close_and_measure(window, panel, trigger(window, "Room output 08"), "empty")
        check(dock_identity(window)[0] == handle, "window resize replaced the dock")
        assert_view(window, "empty")
        expect()
    print("PASS empty-dock popup stays anchored while open through window resize", flush=True)


def lifecycle(window, update, expect):
    def assert_modes():
        children = window.subtree(player_bar(window))
        for label, description in (("Shuffle", "On"), ("Repeat", "Repeat track")):
            control = named(window, children, label)
            check(control and control.get("accessibleEnabled") and control.get("accessibleDescription") == description,
                  f"{label} lost its enabled state or current mode during play/clear")

    update("history")
    update("clear-track")
    update("mixed")
    update("modes-set")
    assert_modes()
    dock = assert_empty_dock(window)
    switch = trigger(window, DESKTOP_OUTPUT[1])
    window.click_element(switch)
    open_panel(window)
    update("play-track")
    assert_view(window, "playing")
    assert_modes()
    check(dock_identity(window) == dock and bounds(trigger(window)) == bounds(switch),
          "starting playback moved or replaced the dock or moved the Devices trigger")
    open_panel(window)
    window.key(TAB)
    window.key(TAB)
    window.key(RETURN)  # Focus still starts at Close, then reaches choice and Rename.
    window.find("Device name", roles=("TextInput",))
    window.shortcut(CONTROL, "a")
    window.key("Retain my draft")
    update("clear-track")
    assert_empty_dock(window)
    assert_modes()
    pane = element_id(window.elements(), "queue-pane")
    check(pane, "clearing playback closed the wide queue pane")
    window.find(r"The queue is empty\..*", roles=TEXT, within=pane)
    open_panel(window)
    field = window.find("Device name", roles=("TextInput",))
    check(field.get("accessibleValue") == "Retain my draft", "clearing playback destroyed the open device editor")
    window.key("!")
    check(window.find("Device name", roles=("TextInput",)).get("accessibleValue") == "Retain my draft!",
          "clearing playback stole focus from the device editor")
    window.key(ESCAPE)
    selected_row(window, DESKTOP_OUTPUT[1])
    current_switch = trigger(window, DESKTOP_OUTPUT[1])
    check((current_switch["handle"], bounds(current_switch)) == (switch["handle"], bounds(switch))
          and dock_identity(window) == dock, "play and clear replaced or moved the dock or the Devices trigger")
    close_and_measure(window, open_panel(window), current_switch, "cleared")
    window.key(RETURN)
    open_panel(window)
    update("change-page-key")
    closed(window)
    window.click_element(trigger(window))
    update("play-track")
    open_panel(window)
    update("now")
    closed(window)
    assert_view(window, "now")
    check(dock_identity(window) == dock, "entering Now Playing moved or replaced the dock")
    window.click_element(trigger(window))
    open_panel(window)
    update("clear-track")
    page = wait("empty Now Playing", lambda: element_type(window.elements(), "NowPlaying"))
    window.find("Nothing playing", roles=TEXT, within=page)
    check(assert_empty_dock(window) == dock, "clearing playback in Now Playing moved or replaced the dock")
    open_panel(window)
    update("history")
    closed(window)
    update("play-track")
    search = "Search tracks, albums or artists"
    query = "Retain this search"
    window.fill(search, query)
    window.click_element(trigger(window))
    open_panel(window)
    update("open-editor")
    closed(window)
    window.find("Playlist name", roles=("TextInput",))
    window.key("Rename fixture playlist")
    check(window.find("Playlist name", roles=("TextInput",)).get("accessibleValue") == "Rename fixture playlist",
          "closing the popup on editor entry did not leave focus in the playlist editor")
    check(dock_identity(window) == dock, "opening a playlist editor moved or replaced the dock")
    locked = window.find(TRIGGER, roles=BUTTON, within=player_bar(window))
    check(not locked.get("accessibleEnabled"), "playlist editor leaves the dock's Devices trigger enabled")
    field = window.find(search, roles=("TextInput",))
    check(not field.get("accessibleEnabled") and field.get("accessibleValue") == query,
          "opening the playlist editor did not lock Search while preserving its query")
    update("close-editor")
    assert_view(window, "playing")
    closed(window)
    field = window.find(search, roles=("TextInput",))
    check(field.get("accessibleEnabled") and field.get("accessibleValue") == query,
          "closing the playlist editor did not restore Search with its query intact")
    window.click("Clear search")
    check(dock_identity(window) == dock, "closing a playlist editor moved or replaced the dock")
    expect()
    print("PASS Devices preserves popup, draft and focus through play/clear; page or editor changes dismiss it", flush=True)

    def focus_dock_lyrics():
        window.click_element(trigger(window))
        open_panel(window)
        window.key(ESCAPE)
        closed(window)
        window.shortcut("\x10", TAB)  # Shift+Tab to Queue, then Lyrics.
        window.shortcut("\x10", TAB)

    focus_dock_lyrics()
    window.key(RETURN)
    expect(("navigate", "now"))  # Establish the keyboard sequence really reaches Lyrics.
    assert_view(window, "now")
    window.click("Lyrics", within=player_bar(window))
    expect()  # An already active Now Playing page must not navigate again.
    window.click("Back")
    expect(("navigate", "back"))
    assert_view(window, "playing")
    check(dock_identity(window) == dock, "Lyrics or Back moved or replaced the dock")
    print("PASS dock Lyrics opens Now Playing once and does not add a duplicate route", flush=True)

    focus_dock_lyrics()
    update("clear-track")
    assert_empty_dock(window)
    window.key(RETURN)
    expect()  # Return must not activate the now-disabled Lyrics control.
    assert_view(window, "empty")
    check(dock_identity(window) == dock, "clearing the focused Lyrics control moved or replaced the dock")
    print("PASS clearing playback prevents Return from activating the previously focused Lyrics control", flush=True)

    region = player_bar(window)
    window.click("Unmute desktop playback", within=region)
    window.find("Mute desktop playback", roles=BUTTON, within=region)
    window.click("Mute desktop playback", within=region)
    slider = window.find("Desktop volume", roles=("Slider",), within=region)
    x, y, right, bottom = bounds(slider)
    window.mcp.call("drag_element", {"elementHandle": slider["handle"],
                                    "target": {"x": x + (right - x) * 0.25, "y": (y + bottom) / 2}})
    window.find("Mute desktop playback", roles=BUTTON, within=region)
    check(dock_identity(window) == dock, "adjusting desktop volume moved or replaced the empty dock")
    expect()
    print("PASS empty dock keeps Mute and volume drag usable without a playback device callback", flush=True)


def roster_states(window, update, expect):
    update("clear-track")
    assert_empty_dock(window)
    for state, max_height in (("empty", 86), ("one", 94)):
        update(state)
        window.click(TRIGGER)
        panel = open_panel(window)
        assert_popup_guides(window, panel)
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
        assert_popup_guides(window, open_panel(window))
        if message:
            window.title(message)
            selected = window.title(DESKTOP_OUTPUT[1])  # stale roster is deliberately supplied by the fixture
            check(not (selected.get("accessibleDescription") or "").startswith("Selected output"), "fake offline selection")
        else:
            trigger(window, "Output disconnected")
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


def dense_guides(window, update, expect):
    update("density-max")
    for width, height in (MINIMUM, WIDE):
        resize(window, (width, height))
        if (width, height) == MINIMUM:
            minimum_rail_settings(window, update, expect, 125)
        update("long-names")
        long_current = DESKTOP_OUTPUT[1] + " — " + "extended label " * 12
        long_alternative = STUDIO_OUTPUT[1] + " — " + "extended label " * 12
        window.click(TRIGGER)
        assert_popup_guides(window, open_panel(window))
        selected_row(window, long_current)
        window.find(option(long_alternative), roles=BUTTON)
        window.pixels(f"devices-long-names-density-125-{width}", width)
        window.click(rename(long_current))
        field = window.find("Device name", roles=("TextInput",))
        check(field.get("accessibleValue") == long_current, "rename did not preserve the full long name")
        assert_popup_guides(window, open_panel(window), stacked_editor=True)
        window.shortcut(CONTROL, "a")
        window.key(DESKTOP_OUTPUT[1])
        window.pixels(f"devices-edit-density-125-{width}", width)
        if width == MINIMUM[0]:
            # The visual rows must not change keyboard order, or activate a write on Cancel.
            window.key(TAB)
            window.key(TAB)
            window.key(RETURN)
            selected_row(window, long_current)
            expect()
        else:
            window.click("Save device name")
            selected_row(window, DESKTOP_OUTPUT[1])
            expect(("device", "rename", DESKTOP_OUTPUT[0], DESKTOP_OUTPUT[1]))
        window.key(ESCAPE)
        update("selected-unavailable")
        window.click(TRIGGER)
        panel = open_panel(window)
        assert_popup_guides(window, panel)
        window.absent(rename(DESKTOP_OUTPUT[1]), roles=BUTTON)
        reason = element_id(window.subtree(panel), "reason-text")
        check(reason and reason.get("accessibleLabel") == "Not available", "missing selected-row reason")
        window.key(ESCAPE)
    expect()
    print("PASS popup guides at 125% text density: long names, editor and unavailable selected row", flush=True)


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
            for size, entry, keyboard in ((MINIMUM, "empty", True), (MINIMUM, "now", True), (WIDE, "playing", False)):
                resize(window, size)
                update("history")
                update("clear-track" if entry == "empty" else "play-track")
                if entry == "empty":
                    minimum_rail_settings(window, update, expect, 100)
                if entry == "now":
                    update("now")
                update("mixed")
                select_journey(window, size[0], entry, DESKTOP_OUTPUT, STUDIO_OUTPUT, keyboard, expect)
                if entry != "now":
                    rename_journey(window, size[0], entry, STUDIO_OUTPUT, f"Studio {entry} {size[0]}", keyboard, expect)
            update("history")
            update("clear-track")
            update("long")
            long_roster(window, expect)
            live_resize(window, expect)
            lifecycle(window, update, expect)
            resize(window, MINIMUM)
            roster_states(window, update, expect)
            dense_guides(window, update, expect)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        sys.exit(f"FAIL: {error}")
