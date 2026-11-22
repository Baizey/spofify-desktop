#!/usr/bin/env python3
r"""Real-binary Desktop manual-add review on private Cage/Slint and synthetic wire.

Build: SLINT_EMIT_DEBUG_INFO=1 cargo build --offline --locked --features slint/mcp
Run in an isolated worker with both exact worker-local TCP ports approved:
  python3 -B scripts/acceptance/manual_add.py --desktop /path/to/debug/spofify-desktop \
    --capture-dir .tmp/search-bulk-add/<new-run>
Pass --before with the base binary to capture the legacy chooser for comparison.
Requires Cage/Wayland, wlr-randr, and ImageMagick. No real account, Go, device,
audio, or installed state. The supplied binary is the actual application.
"""

import argparse
from hashlib import file_digest
import json
from pathlib import Path
import re
import shutil
import signal
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import monotonic, sleep

sys.dont_write_bytecode = True
from catalog_continuity import View as CatalogView, query
from fixture import Failure, check, private_env
from manual_add_wire import EMPTY, LONG, NORMAL, QUERY, READ_ONLY, RULES, SHORT, STUDIES, STUDIES_QUERY, Wire
from native import ESCAPE, RETURN, assert_disjoint, bounds, contains, desktop, element_id, element_type, wait
from settings import resize

TYPICAL, CONSTRAINED = (1280, 790), (640, 790)
PICKER = "Add to playlist: "
UNSELECTED = PICKER + "No destination"
LEGACY_FILTER, SKIP = "Hide already added", "Skip already added"
CLEAR, MEMBER = "Clear selection", "In playlist"
ADD = "Add songs"
GOLD = bytes.fromhex("dfbd78")


def midpoint(box):
    return (box[0] + box[2]) / 2, (box[1] + box[3]) / 2


def aligned(a, b, message, tolerance=1):
    check(abs(a - b) <= tolerance, f"{message}: {a:g} vs {b:g}")


def capture_directory(path):
    scratch = (Path(__file__).resolve().parents[2] / ".tmp").resolve()
    legacy = (Path.home() / "repos/spofify-desktop-temporary/manual-add-production").resolve()
    output = path.resolve()
    check(any(root != output and output.is_relative_to(root) for root in (scratch, legacy)),
          f"use a new evidence directory under {scratch}")
    output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    output.mkdir(mode=0o700)
    return output


class Review(CatalogView):
    def __init__(self, window, output, magick, *, before=False, track_only=False, legacy_member_name=False):
        self.window, self.output, self.magick, self.before = window, output, magick, before
        self.track_only = track_only
        self.legacy_member_name = before or legacy_member_name
        self.timeline = output / "observations.jsonl"

    def nodes(self):
        return self.window.elements()

    def node(self, label, role=None, *, nodes=None):
        nodes = self.nodes() if nodes is None else nodes
        return next((e for e in nodes if e.get("accessibleLabel") == label
                     and (role is None or e.get("accessibleRole") == role)), None)

    def require(self, label, role=None, *, timeout=5):
        return wait(label, lambda: self.node(label, role), timeout)

    def part(self, name, nodes=None):
        element = element_id(self.nodes() if nodes is None else nodes, name)
        check(element, f"missing inspected element {name}")
        return element

    def viewport(self, elements=None):
        return bounds(self.part("catalog-scroll", elements))

    def field(self, nodes=None):
        return self.node("Search tracks, albums or artists", "TextInput", nodes=nodes)

    def value(self, nodes=None):
        return (self.field(nodes) or {}).get("accessibleValue", "")

    def expect_query(self, value):
        wait(f"Search field contains {value!r}", lambda: self.value() == value, 3)

    def park(self):
        self.window.mcp.call("move_pointer", {"windowHandle": self.window.handle,
                                              "position": {"x": 4, "y": 5}})

    def frame(self, name=None):
        data = super().frame(name)
        check(data.startswith(b"\x89PNG\r\n\x1a\n") and data[12:16] == b"IHDR"
              and struct.unpack("!II", data[16:24]) == self.window.dimensions(), "invalid native PNG dimensions")
        return data

    def record(self, label, **data):
        with self.timeline.open("a") as out:
            print(json.dumps(dict(time=monotonic(), label=label, **data), sort_keys=True), file=out)

    def capture(self, name):
        nodes = self.nodes()
        check(not any(n.get("accessibleLabel") in ("Username", "Password") for n in nodes),
              "refusing to capture sign-in")
        self.frame(name)
        # Keep selected accessibility and geometry fields only. The fixture's
        # query is public; never retain input values or window/process state.
        clean = [{key: node[key] for key in ("accessibleRole", "accessibleLabel", "accessibleDescription",
                  "accessibleEnabled", "accessibleChecked", "accessibleExpanded", "accessibleItemSelected",
                  "accessibleItemCount", "absolutePosition", "size", "typeNamesAndIds") if key in node}
                 for node in nodes]
        (self.output / (name + ".json")).write_text(json.dumps(clean, indent=2) + "\n")
        print(f"CAPTURE {name}: {self.window.dimensions()}", flush=True)

    def sample(self, name, *, count=5):
        # Immediate render samples supplement settled PNGs. Report only
        # sampled transitions; this doesn't claim full-refresh video coverage.
        for i in range(count):
            self.frame(f"{name}-t{i:02}")
            nodes = self.nodes()
            selector = next((e for e in nodes if (e.get("accessibleLabel") or "").startswith(PICKER)), None)
            field = self.field(nodes)
            scroll = element_id(nodes, "catalog-scroll")
            self.record(name, sample=i, query=self.value(nodes), selector=(selector or {}).get("accessibleLabel"),
                        field=bounds(field) if field else None, viewport=bounds(scroll) if scroll else None,
                        rows=self.menu_boxes(nodes),
                        title=[e["accessibleLabel"] for e in nodes if e.get("accessibleRole") == "Text"
                               and e.get("accessibleLabel") in ("Search", LONG, "No matching results")])
            sleep(.04)

    def search(self):
        self.window.title("Search")
        # Heading and debounced query publish before a nonempty result model
        # mounts. Require the actual ordinary Search list once rows arrive.
        def loaded():
            nodes = self.nodes()
            return next((item for item in nodes if item.get("accessibleRole") == "List"
                         and item.get("accessibleLabel") == "Search"
                         and item.get("accessibleItemCount", 0) > 0), None)
        return wait("normal Search results list mounted", loaded, 8)

    def select_label(self, name=None):
        return PICKER + name if name else UNSELECTED

    def picker(self, name=None):
        return self.require(self.select_label(name), "Button")

    def popup(self, *, opened_from=None):
        self.window.click_element(self.picker(opened_from))
        return self.require("Add to playlist", "List")

    def set_skip(self, checked):
        switch = self.require(SKIP, "Checkbox")
        check(switch.get("accessibleDescription") == "Album and artist adds only", "skip scope is not named")
        if switch.get("accessibleChecked", False) != checked:
            self.window.click_element(switch)
        return wait(f"skip checked={checked}", lambda: self.destination_skip(checked), 4)

    def destination_skip(self, checked):
        switch = self.node(SKIP, "Checkbox")
        return switch if switch and switch.get("accessibleChecked", False) == checked else None

    def legacy_toggle(self, checked):
        self.window.click_element(self.require(LEGACY_FILTER, "Checkbox"))
        return wait(f"legacy filter checked={checked}", lambda: self.legacy_checked(checked), 4)

    def legacy_checked(self, checked):
        button = self.node(LEGACY_FILTER, "Checkbox")
        return button if button and button.get("accessibleChecked", False) == checked else None

    def layout(self, group, target=None):
        nodes = self.nodes()
        toolbar, field = self.part("toolbar", nodes), self.part("search", nodes)
        picker = self.node(self.select_label(target), "Button", nodes=nodes)
        check(picker, "no fully named destination trigger")
        tb, fb, pb = map(bounds, (toolbar, field, picker))
        for label, rect in (("field", field), ("trigger", picker)):
            check(contains(toolbar, rect), f"{label} escapes toolbar")
        assert_disjoint([("Search field", field), ("destination", picker)])
        aligned(midpoint(fb)[1], midpoint(pb)[1], "Search field / trigger center")
        aligned(midpoint(pb)[1], midpoint(tb)[1], "trigger / toolbar center")
        aligned(pb[0] - fb[2], 8, "field / trigger gap")
        aligned(fb[0] - tb[0], tb[2] - pb[2], "shared toolbar outer insets")
        self.record(group, toolbar=tb, field=fb, trigger=pb, viewport=self.viewport(nodes))
        return tb, fb, pb

    def popup_layout(self, label, target=None):
        panel = self.require("Add to playlist", "List")
        nodes = self.window.subtree(panel, max_elements=500)
        # Use descendant bounds from the same popup coordinate system. Slint
        # inspection may expose these locally; right edge is reviewed in PNG.
        title = next((e for e in nodes if element_type([e], "Caption")
                      and abs(bounds(e)[1] - bounds(panel)[1] - 1) <= 1
                      and bounds(e)[3] - bounds(e)[1] >= 40), None)
        check(title, "missing visible popup title caption")
        rows = [e for e in nodes if e.get("accessibleRole") in ("ListItem", "Button")
                and e.get("accessibleLabel") in (SHORT, LONG, EMPTY)]
        check(len(rows) == 3, f"chooser does not contain exactly three eligible fixture destinations: {rows}")
        check(not any(READ_ONLY in (e.get("accessibleLabel") or "") or RULES in (e.get("accessibleLabel") or "")
                      for e in nodes), "chooser exposed a rule or read-only destination")
        names = [self.part("label", self.window.subtree(row)) for row in rows]
        for name in names:
            aligned(bounds(title)[0], bounds(name)[0], "popup title / destination text guide")
        data = dict(panel=bounds(panel), title=bounds(title), rows=[bounds(e) for e in rows],
                    destination_names=[bounds(e) for e in names])
        if target:
            clear = self.require(CLEAR, "Button")
            clear_box = bounds(clear)
            check(bounds(title)[3] <= clear_box[1] < clear_box[3] <= min(bounds(row)[1] for row in rows),
                  "clear selection must be grouped between the popup title and destination rows")
            data["clear"] = clear_box
        else:
            check(not self.node(LEGACY_FILTER) and not self.node(CLEAR), "no target popup should not show clear/hide")
        if self.track_only:
            check(not self.node(LEGACY_FILTER) and not self.node(SKIP), "unexpected filter in a track-only snapshot")
            if target:
                caption = self.part("clear-label", self.window.subtree(clear))
                aligned(bounds(title)[0], bounds(caption)[0], "popup title / clear / destination text guide")
                aligned(midpoint(clear_box)[1], midpoint(bounds(caption))[1], "clear control / caption center")
        elif not self.before:
            skip = self.require(SKIP, "Checkbox")
            skip_caption = self.part("label", self.window.subtree(skip))
            aligned(bounds(title)[0], bounds(skip_caption)[0], "popup title / skip / destination text guide")
            data["skip"] = bounds(skip)
            if target:
                clear_caption = self.part("label", self.window.subtree(clear))
                assert_disjoint([(SKIP, skip), (CLEAR, clear)])
                aligned(midpoint(data["skip"])[1], midpoint(clear_box)[1], "skip / clear control centers")
                aligned(midpoint(bounds(skip_caption))[1], midpoint(bounds(clear_caption))[1], "skip / clear text baseline")
            check(not self.node(LEGACY_FILTER), "legacy Search filtering unexpectedly present")
        self.record(label, **data)
        return data

    def row_menu(self, item, *, timeout=5):
        return self.require("Show actions for " + item["name"], "Button", timeout=timeout)

    def row_add(self, item, *, pending=False):
        verb = "Adding" if pending else "Add"
        return self.require(f"{verb} {item['name']} to {LONG}", "Button")

    def member(self, item, *, nodes=None):
        nodes = self.nodes() if nodes is None else nodes
        label = MEMBER + ": " + LONG if self.legacy_member_name else MEMBER
        control = next((e for e in nodes if e.get("accessibleLabel") == label
                        and e.get("accessibleDescription") == item["name"]
                        and e.get("accessibleRole") == "Button"), None)
        if control:
            check(not control.get("accessibleEnabled", False), "membership must not be actionable")
        return control

    def visible_menus(self, nodes=None):
        nodes = self.nodes() if nodes is None else nodes
        vp = element_id(nodes, "catalog-scroll")
        return {e["accessibleLabel"].removeprefix("Show actions for "): e for e in nodes
                if vp and (e.get("accessibleLabel") or "").startswith("Show actions for ") and contains(vp, e)}

    def menu_boxes(self, nodes=None):
        return {name: bounds(item) for name, item in self.visible_menus(nodes).items()}

    def settled_menu_boxes(self):
        previous = None
        def settled():
            nonlocal previous
            current = self.menu_boxes()
            same = current == previous
            previous = current
            return current if same else None
        return wait("stable visible Search overflow control bounds", settled, 4)

    def gold(self, control):
        x, y, r, b = bounds(control)
        pixels, width, height = self.rgb(self.frame(), (x + 10, y, r - 10, b))
        def gold_at(line):
            start = line * width * 3
            return sum(pixels[k:k + 3] == GOLD for k in range(start, start + width * 3, 3)) >= width * .65
        return any(gold_at(i) for i in range(4)) and any(gold_at(height - i - 1) for i in range(4))

    def input_query(self, value):
        query(self.window, value)
        self.expect_query(value)
        self.search()

    def select_playlist_row(self, name):
        choice = self.require(name, "ListItem")
        self.window.click_element(choice)
        self.picker(name)
        check(not self.node("Add to playlist", "List"), "chooser did not dismiss on selection")


def popup_cases(view, size):
    w = view.window
    prefix = str(size[0])
    view.input_query(QUERY)
    view.row_menu(NORMAL[0])
    view.park()
    no_target = view.layout(prefix + " no target")
    check(not any((e.get("accessibleLabel") or "").startswith("Add " + NORMAL[0]["name"])
                  for e in view.nodes()), "quick-add visible without a selected destination")
    view.capture(prefix + "-01-no-target")
    view.require(f"{len(NORMAL)} results", "Text")
    original_rows = view.settled_menu_boxes()
    view.popup()
    view.capture(prefix + "-02-no-target-popup")
    view.popup_layout(prefix + " no target popup")
    view.select_playlist_row(LONG)
    wait("seeded membership of selected long destination", lambda: view.member(NORMAL[4]), 8)
    view.row_add(NORMAL[0])
    view.park()
    long_target = view.layout(prefix + " full long target", LONG)
    aligned(no_target[2][2], long_target[2][2], "picker right edge after selecting long destination")
    check(view.viewport()[1] >= no_target[0][3], "selected Search viewport overlaps the toolbar")
    w.absent(re.escape(f"Add {NORMAL[2]['name']} to {LONG}"), roles=("Button",))
    view.capture(prefix + "-03-long-target")
    selected_rows = view.settled_menu_boxes()
    vp = view.viewport()
    expected = {name: box for name, box in original_rows.items() if vp[1] <= box[1] and box[3] <= vp[3]}
    check(selected_rows == expected, "target selection removed or moved a result within the available viewport")
    view.popup(opened_from=LONG)
    view.capture(prefix + "-04-selected-popup")
    view.popup_layout(prefix + " selected popup", LONG)
    if view.before:
        check(view.legacy_checked(False), "base filter should initially be off")
        view.legacy_toggle(True)
        view.capture(prefix + "-05-legacy-hide-popup")
        w.absent(re.escape(f"{len(NORMAL)} results"), roles=("Text",))
    elif not view.track_only:
        switch = view.require(SKIP, "Checkbox")
        initial = switch.get("accessibleChecked", False)
        view.set_skip(not initial)
        check(view.popup_layout(prefix + " skip toggled", LONG)["skip"] == bounds(switch), "skip control moved")
        check(view.settled_menu_boxes() == selected_rows and view.member(NORMAL[4]),
              "bulk skip setting affected track rows")
        view.capture(prefix + "-05-bulk-skip-toggled")
        view.set_skip(initial)
    w.key(ESCAPE)
    view.expect_query(QUERY)
    active_rows = view.settled_menu_boxes()
    if view.before:
        check(NORMAL[4]["name"] in selected_rows and NORMAL[4]["name"] not in active_rows,
              "base filter did not hide the initially visible member")
    else:
        check(NORMAL[4]["name"] in active_rows and active_rows == selected_rows,
              "target selection or chooser dismissal removed or moved a Search hit")
        view.require(f"{len(NORMAL)} results", "Text")
    view.capture(prefix + "-06-selected-dismissed")
    check(view.layout(prefix + " selected dismissed", LONG) == long_target, "chooser moved sticky toolbar")
    view.popup(opened_from=LONG)
    w.click_element(view.require(CLEAR, "Button"))
    view.sample(prefix + "-clear")
    view.picker()
    view.expect_query(QUERY)
    view.row_menu(NORMAL[4])
    check(view.layout(prefix + " cleared") == no_target, "clear did not restore no-target toolbar geometry")
    check(view.settled_menu_boxes() == original_rows, "clear did not restore the original result viewport")
    view.capture(prefix + "-07-cleared")
    print(f"PASS {prefix}px chooser, full accessible name, eligibility, membership visibility and clear", flush=True)


def manual_route(view):
    w = view.window
    w.click("Playlists")
    for unwanted in (READ_ONLY, RULES):
        w.click(re.escape("Open " + unwanted))
        w.title(unwanted)
        w.absent(re.escape(ADD), roles=("Button",))
        w.click("Back")
    w.click(re.escape("Open " + LONG))
    w.title(LONG)
    action = view.require(ADD, "Button")
    left, top, right, bottom = bounds(action)
    port_left, port_top, port_right, port_bottom = view.viewport()
    check(port_left <= left < right <= port_right and port_top <= top < bottom <= port_bottom,
          "Add songs is clipped in the playlist viewport")
    view.park()
    view.capture("manual-01-initiating-route")
    search_field = bounds(view.part("search"))
    w.click_element(action)
    view.sample("manual-entry")
    w.title("Search")
    view.picker(LONG)
    # Prove focus by typing without clicking the field or using a focus shortcut.
    check(view.field(), "missing the normal Search field")
    view.capture("manual-02a-search-entry-focused")
    field_value = view.value()
    check(field_value in ("", QUERY), "manual entry populated an unexpected query")
    view.record("manual entry focus", initial_query=field_value)
    w.key(QUERY if field_value == "" else "x")
    view.expect_query(QUERY if field_value == "" else QUERY + "x")
    if field_value:
        w.key("\x08")
        view.expect_query(QUERY)
    view.search()
    check(bounds(view.part("search"))[0] == search_field[0], "manual entry moved the normal Search leading edge")
    view.row_menu(NORMAL[0])
    view.capture("manual-02-normal-search-focused")
    w.click("Back")
    view.sample("manual-back")
    w.title(LONG)
    action = view.require(ADD, "Button")
    wait("visible focus restored to Add songs", lambda: view.gold(action), 3)
    view.capture("manual-03-back-focus")
    w.key(RETURN)
    w.title("Search")
    view.picker(LONG)
    if view.value() == "":
        w.key(QUERY)
    view.expect_query(QUERY)
    view.search()
    view.row_menu(NORMAL[0])
    print("PASS Add songs enters normal Search with input focus; Back returns to initiating detail with Enter working", flush=True)


def quick_add(view, wire):
    w = view.window
    canary = NORMAL[5]
    toolbar = view.layout("before scroll", LONG)
    view.scroll(-255)
    wait("scroll actually changed the visible answer window", lambda: NORMAL[0]["name"] not in view.visible_menus(), 4)
    previous_boxes = view.settled_menu_boxes()
    menus = view.visible_menus()
    check(len(menus) >= 3 and canary["name"] in menus, "write/anchor rows are not in the scrolled viewport")
    plus, more = view.row_add(canary), menus[canary["name"]]
    assert_disjoint([("quick add", plus), ("overflow", more)])
    aligned(midpoint(bounds(plus))[1], midpoint(bounds(more))[1], "plus / overflow centers")
    aligned(midpoint(bounds(more))[0] - midpoint(bounds(plus))[0], 40, "adjacent plus and overflow centers")
    check(view.layout("after scroll", LONG) == toolbar, "Search toolbar moved while results scrolled")
    view.park()
    view.capture("write-01-scrolled-ready")
    ordinal = wire.expect_add(canary)
    w.click_element(plus)
    try:
        wait("wire addEntry for the displayed track and target", lambda: wire.exact_add(ordinal, canary), 4)
        pending = view.row_add(canary, pending=True)
        # Slint's protobuf JSON elides a false accessibleEnabled value.
        check(not pending.get("accessibleEnabled", False), "pending slot is still enabled")
        check(bounds(pending) == bounds(plus), "pending indicator shifted the add slot")
        view.park()
        view.capture("write-02-held-pending")
        view.expect_query(QUERY)
        check(view.menu_boxes() == previous_boxes, "viewport jumped while add was pending")
        w.click_element(pending)
    finally:
        wire.confirm.set()
    view.sample("write-confirmation")
    wait("accepted membership replaced pending", lambda: view.member(canary), 6)
    view.park()
    view.capture("write-03-confirmed")
    member = view.member(canary)
    check(bounds(member) == bounds(plus), "confirmed membership shifted the add slot")
    check(view.layout("after confirmation", LONG) == toolbar, "toolbar moved on membership confirmation")
    view.expect_query(QUERY)
    check(view.menu_boxes() == previous_boxes, "viewport jumped on membership confirmation")
    wait("fixture sent authoritative echo", lambda: wire.seen("add-confirmed", ordinal=ordinal))
    check(wire.exact_add(ordinal, canary), "not exactly one observed add and echo")
    print("PASS sticky toolbar, plus/pending/check slots, one addEntry, and preserved scrolled query and viewport", flush=True)


def member_continuation(view, wire):
    view.input_query(STUDIES_QUERY)
    wait("visible seeded member at the start of Search", lambda: view.member(STUDIES[0]), 5)
    check(STUDIES[0]["name"] in view.visible_menus(), "known-member prefix disappeared from Search")
    check(not wire.seen("search", query=STUDIES_QUERY, page=2), "continuation started before scrolling")
    view.no_pagers()
    view.park()
    view.capture("continuation-01-seeded-member-prefix")
    try:
        view.scroll(-2000)
        wait("scroll requests the next Search cursor", lambda: wire.seen("search", query=STUDIES_QUERY, page=2), 5)
        check(STUDIES[0]["name"] not in view.visible_menus(), "Search viewport did not advance")
        check(STUDIES[18]["name"] in view.visible_menus() and view.member(STUDIES[18]),
              "first page's tail members vanished while continuation was held")
        view.park()
        view.capture("continuation-02-scrolled-cursor-held")
    finally:
        wire.continue_studies.set()
    appended = STUDIES[-1]
    # The list reports all accepted rows before the last row is mounted in
    # Slint's virtualized viewport (and before it fits the root tree budget).
    wait("full Search continuation is accepted", lambda: any(
        node.get("accessibleItemCount") == len(STUDIES)
        for node in view.dom_items("AppWindow::catalog-results")), 5)
    view.no_pagers()
    view.scroll(-650)
    wait("appended row fully in viewport", lambda: appended["name"] in view.visible_menus(), 5)
    check(len(wire.seen("search", query=STUDIES_QUERY, page=2)) == 1, "continuation duplicated")
    previous_rows = view.settled_menu_boxes()
    check(STUDIES[-2]["name"] in previous_rows and view.member(STUDIES[-2]),
          "appended member beside the ready row is not visible")
    plus = view.row_add(appended)
    check(plus.get("accessibleEnabled"), "appended quick-add is not ready")
    ordinal = wire.expect_add(appended)
    try:
        view.window.click_element(plus)
        wait("write uses the appended source track and exact target", lambda: wire.exact_add(ordinal, appended), 4)
        pending = view.row_add(appended, pending=True)
        check(bounds(pending) == bounds(plus) and not pending.get("accessibleEnabled", False),
              "appended row's pending control changed slots or remained enabled")
        view.park()
        view.capture("continuation-03-appended-pending")
    finally:
        wire.confirm.set()
    member = wait("appended track stays visible as a confirmed member", lambda: view.member(appended), 6)
    check(bounds(member) == bounds(plus) and view.menu_boxes() == previous_rows,
          "confirmation removed or moved the scrolled row or its neighbors")
    view.expect_query(STUDIES_QUERY)
    view.accepted_results(len(STUDIES))
    wait("second authoritative echo", lambda: wire.seen("add-confirmed", ordinal=ordinal))
    check(wire.exact_add(ordinal, appended), "appended add was duplicated")
    view.capture("continuation-04-appended-confirmed")
    print("PASS live continuation through known members, exact appended write, and persistent confirmed row", flush=True)


def explicit_repeat(view, wire):
    # Optional compatibility guard. Use the same recording already added by
    # the scrolled quick-add scenario, not a different target or seeded member.
    w = view.window
    canary = NORMAL[5]
    view.input_query(QUERY)
    wait("quick toolbar shows membership for the same recording", lambda: view.member(canary), 6)
    if canary["name"] not in view.visible_menus():
        view.scroll(-255)
        wait("explicit repeat row fully visible", lambda: canary["name"] in view.visible_menus(), 4)
    w.click_element(view.row_menu(canary))
    w.click(re.escape("Add to playlist · " + canary["name"]))
    w.title("Add to playlist")
    destination = view.require("Add to " + LONG, "Button")
    view.capture("repeat-01-normal-picker")
    ordinal = wire.expect_add(canary)
    try:
        w.click_element(destination)
        wait("explicit repeat writes exactly the same track and target", lambda: wire.exact_add(ordinal, canary), 5)
    finally:
        wire.confirm.set()
    current = wait("explicit repeat confirmed as a new occurrence",
                   lambda: wire.seen("add-confirmed", ordinal=ordinal), 6)[0]
    original = wire.seen("add-confirmed", ordinal=1)[0]
    check(current["track"] == original["track"] and current["occurrence"] != original["occurrence"],
          "explicit repeat did not create a separate occurrence")
    check(wire.exact_add(ordinal, canary), "explicit repeat was duplicated")
    w.title(LONG)
    view.capture("repeat-02-explicit-repeat-confirmed")
    print("PASS ⋯ normal Add to playlist permits one intentional repeat despite quick membership", flush=True)


def login(window, ready):
    window.find("Sign in", roles=("Button",))
    window.fill("Server( origin| URL| address)?", ready["origin"])
    window.fill("Username", ready["username"])
    window.fill("Password", ready["password"])
    window.click("Sign in")
    try:
        window.section("History")
    except Failure as error:
        reach = any(e.get("accessibleLabel") == "Unable to reach the server. Check your connection and server address."
                    for e in window.elements())
        raise Failure("disposable sign-in failed; check both worker-local TCP grants" if reach else
                      "disposable sign-in did not reach History") from error


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--desktop", type=Path, required=True)
    parser.add_argument("--capture-dir", type=Path, required=True)
    parser.add_argument("--wire-port", type=int, default=39281)
    parser.add_argument("--mcp-port", type=int, default=39282)
    parser.add_argument("--smoke", action="store_true", help="prove native startup and isolated account sign-in only")
    parser.add_argument("--explicit-repeat", action="store_true", help="also exercise an intentional duplicate via ⋯")
    compare = parser.add_mutually_exclusive_group()
    compare.add_argument("--before", action="store_true", help="compare the pre-change binary and its legacy filter")
    compare.add_argument("--track-only", action="store_true", help="compare a track-only intermediate without a bulk skip setting")
    parser.add_argument("--legacy-member-name", action="store_true", help="capture an interim binary with the old target-inclusive member name")
    args = parser.parse_args()
    check(1024 <= args.wire_port <= 65535 and 1024 <= args.mcp_port <= 65535
          and args.wire_port != args.mcp_port, "specify two distinct unprivileged worker-local ports")
    binary = args.desktop.resolve(strict=True)
    magick = shutil.which("magick") or shutil.which("convert")
    check(magick, "ImageMagick required to check native focus pixels")
    output = capture_directory(args.capture_dir)
    with binary.open("rb") as source:
        (output / "binary.sha256").write_text(file_digest(source, "sha256").hexdigest() + "\n")
    wire = Wire(output, args.wire_port)
    print(f"Evidence: {output}", flush=True)
    with TemporaryDirectory(prefix="adp.", dir="/tmp") as temporary:
        # Linux's UNIX socket path is length-limited. Never use the capture
        # directory as XDG_RUNTIME_DIR or retain the disposable account store.
        root = Path(temporary)
        env = private_env(root)
        no_bus = "unix:path=" + env["XDG_RUNTIME_DIR"] + "/no-bus"
        env.update(DBUS_SESSION_BUS_ADDRESS=no_bus, DBUS_SYSTEM_BUS_ADDRESS=no_bus)
        try:
            with wire.serve() as ready, desktop(binary, root, env, multisize=True, mcp_port=args.mcp_port) as window:
                wire.preflight()
                resize(window, TYPICAL)
                check(window.window_properties()["scaleFactor"] == 1, "require scale-1 native geometry")
                login(window, ready)
                wait("isolated account snapshot", lambda: wire.seen("snapshot"))
                if args.smoke:
                    print("PASS private Cage, native Slint MCP, and synthetic account sign-in", flush=True)
                    return
                view = Review(window, output, magick, before=args.before, track_only=args.track_only,
                              legacy_member_name=args.legacy_member_name)
                try:
                    popup_cases(view, TYPICAL)
                    resize(window, CONSTRAINED)
                    popup_cases(view, CONSTRAINED)
                    resize(window, (480, 790))
                    manual_route(view)
                    resize(window, TYPICAL)
                    quick_add(view, wire)
                    member_continuation(view, wire)
                    if args.explicit_repeat:
                        explicit_repeat(view, wire)
                except (Failure, RuntimeError, OSError, ValueError):
                    try:
                        view.capture("failure-last-visible-state")
                    except (Failure, RuntimeError, OSError, ValueError) as capture_error:
                        print(f"Could not capture failure state: {capture_error}", file=sys.stderr)
                    raise
        finally:
            log = root / "desktop.log"
            if log.exists():
                shutil.copyfile(log, output / "desktop.log")
    print(f"PASS native manual-add fixture: {output}", flush=True)


def interrupted(*_):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        sys.exit(f"FAIL: {error or type(error).__name__}")
