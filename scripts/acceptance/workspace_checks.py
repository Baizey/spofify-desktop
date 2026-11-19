"""Pointer and layout contracts for the listening workspace's native window."""
import json
import re

from fixture import Failure, check
from native import (ALT, CONTROL, DOWN, ESCAPE, LEFT, OFFLINE_SWITCH, OLD_NAV, PLAYLIST,
                    RETURN, TRACK, bounds, element_id, element_type, wait)
from wire_ui import ALBUM_NAME, ARTIST_NAME, GUEST_NAME, ID_ONLY_RELEASE, NAME_ONLY


QUEUE_TITLE = "Queue"
QUEUE_CONTROLS = ("Shuffle", r"Repeat: (off|one|all)")


def open_playlists(window):
    window.click("Playlists")
    window.section("Playlists")
    window.find(f"Open {PLAYLIST}", roles=("Button",))


def open_manual(window):
    window.click(f"Open {PLAYLIST}")
    manual_detail(window)


def manual_detail(window, timeout=10):
    window.title(PLAYLIST, timeout=timeout)
    wait("two distinct playlist occurrences", lambda: sum(
        window.matches(element, f"Open {TRACK}") and element.get("accessibleRole") == "Button"
        for element in window.elements()) >= 2, timeout=timeout)


def manual_options(window, *, capture=None):
    management = r"Rename|Delete|Edit rules"
    window.absent(management, roles=("Button",))
    window.click("Manage playlist")
    trigger = window.find("Hide playlist options", timeout=3, roles=("Button",))
    window.find("Delete", timeout=3, roles=("Button",))
    window.absent("Edit rules", roles=("Button",))
    if capture:
        window.pixels(capture)
    x, y = window.pointer_position_for_click("Rename")
    _, top, right, bottom = bounds(trigger)
    width, height = window.dimensions()
    check(0 < x < width and 0 < y < height and 50 < right - x < 185
          and (0 < y - bottom < 52 or 0 < top - y < 102),
          f"rendered Rename target {x:g},{y:g} is not anchored to playlist menu {bounds(trigger)}")
    window.title("Rename playlist")
    window.find("Playlist name", roles=("TextInput",))
    window.click("Cancel")
    window.title(PLAYLIST)
    window.absent(management, roles=("Button",))
    window.click("Manage playlist")
    window.find("Rename", timeout=3, roles=("Button",))
    window.click("Hide playlist options")
    window.title(PLAYLIST)
    window.absent(management, roles=("Button",))
    print("PASS manual playlist overflow opens, activates Rename and dismisses without navigating", flush=True)


def keyboard_playlist_menu(window):
    # Clicking the trigger transfers focus to the popup; Escape must restore
    # it to that trigger without spending a Back entry. No refocus/fallback.
    window.click("Manage playlist")
    window.find("Rename", timeout=3, roles=("Button",))
    window.key(ESCAPE)
    window.title(PLAYLIST)
    window.absent("Rename|Delete", roles=("Button",))
    window.key(RETURN)
    window.find("Rename", timeout=3, roles=("Button",))
    window.key(DOWN)
    window.key(RETURN)
    window.title("Delete this playlist?")
    window.click("Cancel")
    window.title(PLAYLIST)
    window.absent("Rename|Delete", roles=("Button",))
    print("PASS Escape restores trigger focus, Return reopens, Down/Return opens Delete confirmation", flush=True)


def playlist_row_menu(window):
    add = f"Add to playlist · {TRACK}"
    window.absent(add, roles=("Button",))
    window.click(f"Show actions for {TRACK}")
    window.find(f"Remove · {TRACK}", timeout=3, roles=("Button",))
    window.click(add)
    window.title("Add to playlist")
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    window.click("Back")
    window.title(PLAYLIST)
    window.absent(add, roles=("Button",))
    print("PASS track overflow activates the playlist picker and is dismissed on Back", flush=True)


def rules_editor(window, capture=None):
    window.click("New rules playlist")
    window.title("New rules playlist")
    window.find("Playlist rules definition", roles=("TextInput",))
    window.absent(OFFLINE_SWITCH, roles=("Button", "Checkbox"))
    window.fill("Playlist name", "Cancelled synthetic draft")
    if capture:
        window.pixels(capture)
    window.click("Cancel")
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    window.find("New rules playlist", roles=("Button",))
    window.absent("Playlist rules definition")
    window.absent("Open Cancelled synthetic draft")
    window.absent(OFFLINE_SWITCH, roles=("Button", "Checkbox"))
    print("PASS rules editor has no connection switch and Cancel restores the playlist listing", flush=True)


def committed_playlist_edits(window, api):
    """Drive the native editor; a separate real account socket witnesses each commit."""
    from playback import Peer

    original = "Native acceptance disposable playlist"
    renamed = "Native acceptance renamed playlist"
    phase = "create"
    begin = created = updated = None
    with Peer(api, name="Acceptance mutation observer") as observer:
        try:
            open_playlists(window)
            begin = {p["id"] for p in observer.personal_snapshot["playlists"]}
            window.click("New playlist")
            window.title("New playlist")
            window.fill("Playlist name", original)
            window.click("Create playlist")
            created = observer.personal_change(lambda ch: ch.get("playlist", {}).get("name") == original)["playlist"]
            pid = created["id"]
            check(pid not in begin and created["kind"] == "manual" and created["trackCount"] == 0,
                  "native create did not commit a new empty manual playlist")
            window.title(original)
            window.absent("Cancel|Working…", roles=("Button",))

            phase = "rename"
            window.click("Manage playlist")
            window.click("Rename")
            window.title("Rename playlist")
            editor = wait("rename editor loaded current committed name", lambda: next((e for e in window.elements()
                          if window.matches(e, "Playlist name") and e.get("accessibleRole") == "TextInput"
                          and e.get("accessibleValue") == original), None))
            window.click_element(editor)
            window.shortcut(CONTROL, "a")
            window.key(renamed)
            window.click("Save changes")
            updated = observer.personal_change(lambda ch: ch.get("playlist", {}).get("id") == pid
                                               and ch["playlist"].get("name") == renamed)["playlist"]
            check(updated["revision"] > created["revision"], "native rename did not advance the revision")
            window.title(renamed)
            window.absent("Cancel|Working…", roles=("Button",))
            with Peer(api, name="Acceptance playlist readback") as reader:
                detail = reader.playlist(pid)
                check(detail["summary"] == updated and detail["rows"] == [],
                      "fresh real socket did not read back the renamed playlist")

            phase = "delete"
            window.click("Manage playlist")
            window.click("Delete")
            window.title("Delete this playlist?")
            window.click("Delete playlist")
            observer.personal_change(lambda ch: ch.get("deleted") == pid)
            window.find(f"Open {PLAYLIST}", roles=("Button",))
            window.absent(f"Open {renamed}|Cancel|Working…", roles=("Button",))
            with Peer(api, name="Acceptance deletion readback") as reader:
                check({p["id"] for p in reader.personal_snapshot["playlists"]} == begin,
                      "fresh real socket did not confirm deletion and unchanged peer playlists")
        except (Failure, TimeoutError) as error:
            if window.output:
                # Only synthetic name/identity/revision/count fields. No cookies,
                # admission tickets, player output handles or WebSocket frames.
                def summary(item):
                    return {k: item.get(k) for k in ("id", "name", "kind", "revision", "trackCount")} if item else None

                state = observer.personal_snapshot or {}
                evidence = {"phase": phase, "created": summary(created), "renamed": summary(updated),
                            "playlists": [summary(item) for item in state.get("playlists", [])]}
                with (window.output / "real-playlist-write-failure.json").open("x") as file:
                    json.dump(evidence, file, indent=2)
            raise Failure(f"native playlist {phase}: {error}") from error
    print("PASS native create, rename and delete closed editors; independent deltas and fresh readback confirm commits", flush=True)


def check_navigation(window):
    window.absent(OLD_NAV, roles=("Button",))
    window.absent(OFFLINE_SWITCH, roles=("Button", "Checkbox"))


def search_results(window):
    window.title("Results")
    for title in (ARTIST_NAME, ALBUM_NAME, TRACK):
        window.find(f"Open {title}", roles=("Button",))
    field = window.find("Search tracks, albums or artists", roles=("TextInput",))
    check(field.get("accessibleValue") == "Synthetic", "Back did not restore the typed search query")


def artist(window, name):
    window.title(name)
    window.find(f"Open {ALBUM_NAME}", roles=("Button",))
    if name == ARTIST_NAME:
        window.find(f"Open {ID_ONLY_RELEASE['name']}", roles=("Button",))
    window.absent("This item could not be opened.", roles=("Text",))


def qualified_credits(window):
    targets = [window.find(f"Open artist: {name}", roles=("Button",)) for name in (ARTIST_NAME, GUEST_NAME)]
    targets.append(window.title(NAME_ONLY))
    catalog = element_id(window.elements(), "catalog-scroll")
    check(catalog, "missing catalog viewport for detail credits")
    cx, cy, cright, cbottom = bounds(catalog)
    for target in targets:
        x, y, right, bottom = bounds(target)
        check(cx <= x < right <= cright and cy <= y < bottom <= cbottom,
              f"detail credit is clipped: {target['accessibleLabel']} at {bounds(target)}")
    window.absent(rf"(Open artist: )?{NAME_ONLY}", roles=("Button",))


def containing_item(elements, name, target):
    x, y, right, bottom = bounds(target)
    return next((e for e in elements if element_type([e], name)
                 and bounds(e)[0] <= x < right <= bounds(e)[2]
                 and bounds(e)[1] <= y < bottom <= bounds(e)[3]), None)


def compact_track_row(window):
    opening = window.find(f"Open {TRACK}", roles=("Button",))
    row = containing_item(window.elements(), "MediaLine", opening)
    check(row, "missing compact search-result row for track hit testing")
    return row, window.subtree(row)


def compact_track_face(window, region):
    row, elements = compact_track_row(window)
    # The accessible Open rectangle covers the whole row and its center can
    # coincide with a credit. Target the rendered title or padding directly.
    words = element_id(elements, "words")
    check(words, "missing compact track copy")
    target = (element_type(window.subtree(words), "Text") if region == "title"
              else element_type(elements, "Divider"))
    links = [e for e in elements if e.get("accessibleRole") == "Button"
             and window.matches(e, r"Open (artist|album): .+")]
    check(target and len(links) >= 2, f"missing compact track {region} or its independent metadata links")
    point = window.pointer_position_for_click(f"compact track {region}", element=target)
    check(inside(point, target) and inside(point, row) and not any(inside(point, e) for e in links),
          f"compact track {region} press {point} landed on a metadata link or outside its row")
    window.title(TRACK, timeout=3)


def compact_track_targets(window):
    compact_track_face(window, "bottom padding")
    window.click("Back")
    search_results(window)
    for link, verify_destination in (
        (f"Open artist: {ARTIST_NAME}", lambda: artist(window, ARTIST_NAME)),
        (f"Open album: {ALBUM_NAME}", lambda: window.title(ALBUM_NAME, timeout=3)),
    ):
        row, elements = compact_track_row(window)
        target = next((e for e in elements if e.get("accessibleRole") == "Button" and window.matches(e, re.escape(link))), None)
        check(target, f"missing compact track row link {link}")
        point = window.pointer_position_for_click(link, element=target)
        check(inside(point, target) and inside(point, row), f"{link} press {point} did not hit the compact row link")
        verify_destination()
        window.click("Back")
        search_results(window)
    compact_track_face(window, "title")
    print("PASS compact track title and padding open the track; in-row artist and album links open their own destinations",
          flush=True)


def typed_catalog(window, width, *, capture=False, via_shortcut=False, check_compact_targets=False):
    # This always originates in /search. The fixture rejects local browse text
    # queries, so a legacy catalog lookup cannot accidentally satisfy the test.
    if via_shortcut:
        window.shortcut(CONTROL, "f")
        window.key("Synthetic")
    else:
        window.fill("Search tracks, albums or artists", "Synthetic")
    search_results(window)
    if capture:
        window.pixels(f"search-{width}", width)

    window.click(f"Open {ARTIST_NAME}")
    artist(window, ARTIST_NAME)
    if capture:
        window.pixels(f"artist-{width}", width)
    window.click(f"Open {ID_ONLY_RELEASE['name']}")
    window.title(ID_ONLY_RELEASE["name"])
    window.click("Back")
    artist(window, ARTIST_NAME)
    window.click("Back")
    search_results(window)

    window.click(f"Open {ALBUM_NAME}")
    window.title(ALBUM_NAME)
    window.find(f"Open {TRACK}", roles=("Button",))
    window.click("Back")
    search_results(window)

    if check_compact_targets:
        compact_track_targets(window)
    else:
        window.click(f"Open {TRACK}")
    window.title(TRACK)
    qualified_credits(window)
    window.click(f"Open artist: {GUEST_NAME}")
    artist(window, GUEST_NAME)
    window.click("Back")
    window.title(TRACK)
    window.click(f"Open album: {ALBUM_NAME}")
    window.title(ALBUM_NAME)
    window.find(f"Open {TRACK}", roles=("Button",))
    qualified_credits(window)
    if capture:
        window.pixels(f"album-{width}", width)
    window.click(f"Open artist: {ARTIST_NAME}")
    artist(window, ARTIST_NAME)
    window.click("Back")
    window.title(ALBUM_NAME)
    window.click("Back")
    window.title(TRACK)
    window.click("Back")
    search_results(window)
    open_playlists(window)
    window.absent("Back", roles=("Button",))
    print(f"PASS at {width}px: typed search, ID-only release, named credit routes and Back", flush=True)


def check_shortcut_back(window):
    open_manual(window)
    window.shortcut(ALT, LEFT)
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    window.absent("Back", roles=("Button",))


def search_layout(window, elements, toolbar, *, docked):
    # TextInput excludes the icon and clear affordance; measure the painted Field.
    field = element_id(elements, "search")
    hero = element_id(elements, "detail-hero")
    check(field and hero, "missing rendered Search field or catalog content inset")
    input_target = window.find("Search (tracks, albums or artists|downloads)", roles=("TextInput",))
    brand = window.title("Spofify")
    tx, ty, tright, tbottom = bounds(toolbar)
    x, y, right, bottom = bounds(field)
    ix, iy, iright, ibottom = bounds(input_target)
    check(tx <= x <= ix < iright <= right <= tright and ty <= y <= iy < ibottom <= bottom <= tbottom,
          f"Search or its input leaves the toolbar: {bounds(field)}, {bounds(input_target)}, {bounds(toolbar)}")
    check(abs((y + bottom - ty - tbottom) / 2) <= 1
          and abs((y + bottom - bounds(brand)[1] - bounds(brand)[3]) / 2) <= 10,
          f"Search is not centered in the toolbar and aligned with the rail brand: {bounds(field)}")
    content_left, _, content_right, _ = bounds(hero)
    inset = x - tx
    check(0 < inset <= 40 and abs(x - content_left) <= 2,
          f"Search is not aligned with the catalog gutter: {x:g} vs {content_left:g}")
    usable_right = tright - inset
    check(abs(content_right - usable_right) <= 2,
          f"catalog content does not fill its usable width: ends at {content_right:g}, expected {usable_right:g}")
    tools = element_id(elements, "playback-tools")
    if docked:
        check(abs(right - usable_right) <= 2,
              f"Search leaves unused toolbar space: field ends at {right:g}, usable edge at {usable_right:g}")
    else:
        check(tools, "missing playback shortcuts in the toolbar without a dock")
        cx, cy, cr, cb = bounds(tools)
        check(0 <= cx - right <= 12 and ty <= cy < cb <= tbottom and abs(cr - usable_right) <= 2,
              f"Search and playback shortcuts do not fill the toolbar: {bounds(field)}, {bounds(tools)}")
    return x, right - x


def pane_layout(window, *, wide, docked=False, queue_open=None, can_go_back=False):
    elements = window.elements()
    work = element_id(elements, "work")
    rail = element_id(elements, "rail")
    toolbar = element_id(elements, "toolbar")
    catalog = element_id(elements, "catalog-scroll")
    queue = element_id(elements, "queue-pane")
    dock = element_type(elements, "PlayerBar")
    check(work and rail and toolbar and catalog, "missing work area, navigation rail, toolbar or catalog viewport")
    width, height = window.dimensions()
    x, y, right, bottom = bounds(catalog)
    check(0 <= x < right <= width and 0 <= y < bottom <= height, "catalog viewport leaves client window")
    wx, wy, wright, wbottom = bounds(work)
    rx, ry, rright, rbottom = bounds(rail)
    check(wx == rx == wy == ry == 0 and wright == width
          and abs(rright - x) <= 2 and abs(rbottom - bottom) <= 2 and abs(wbottom - bottom) <= 2,
          f"navigation rail and workspace do not line up: {bounds(rail)}, {bounds(work)}, {bounds(catalog)}")
    if docked:
        check(dock and abs(bottom - bounds(dock)[1]) <= 1, "workspace does not meet the populated dock")
    else:
        check(not dock, "empty state renders a playback dock")
        check(abs(bottom - height) <= 1,
              f"empty state reserves a bottom strip: viewport ends at {bottom:g}, client ends at {height:g}")
        window.absent("No song selected|Now playing: Nothing playing")
    tx, ty, tright, tbottom = bounds(toolbar)
    check(ty == 0 and abs(tx - x) <= 1 and abs(tright - right) <= 1,
          f"toolbar and catalog have different edges: {bounds(toolbar)}, {bounds(catalog)}")
    search = search_layout(window, elements, toolbar, docked=docked)
    if can_go_back:
        back = window.find("Back", roles=("Button",))
        bx, by, br, bb = bounds(back)
        check(abs(bx - search[0]) <= 2 and tbottom <= by < bb <= y and bx < br <= right,
              f"Back does not sit below Search and above the catalog: {bounds(back)}, {bounds(catalog)}")
    else:
        window.absent("Back", roles=("Button",))
        check(abs(tbottom - y) <= 1,
              f"root reserves space for Back: toolbar ends at {tbottom:g}, catalog begins at {y:g}")
    if queue_open is None:
        queue_open = wide
    check(bool(queue) == queue_open, "queue pane visibility does not match the requested layout")
    if not queue:
        check(abs(width - right) <= 2 and right - x >= 0.72 * width,
              f"workspace does not fill the space to the right of the rail: {bounds(catalog)}")
    else:
        qx, qy, qright, qbottom = bounds(queue)
        check(right <= qx < qright <= width and abs(qbottom - bottom) <= 2 and qy == wy,
              f"overlapping or clipped catalog/queue: {bounds(catalog)}, {bounds(queue)}")
        check(right - x >= 0.55 * width and 245 <= qright - qx <= 0.35 * width,
              f"unusable pane widths: catalog {right - x:g}, queue {qright - qx:g}, window {width:g}")
        check(abs(width - qright) <= 2 and abs(qx - right) <= 2 and x <= 0.2 * width,
              "desktop wastes space outside its panes")
    return search


def root_navigation(window, *, wide):
    window.section("History")
    expected_search = pane_layout(window, wide=wide)
    # Exercise the actual rail, including both ways away from Saved offline.
    for label in ("Playlists", "Saved offline", "History", "Saved offline", "Settings", "Playlists"):
        window.click(label)
        window.section(label)
        actual_search = pane_layout(window, wide=wide)
        check(all(abs(a - b) <= 1 for a, b in zip(actual_search, expected_search)),
              f"Search x/width moved on root {label}: {actual_search} vs {expected_search}")
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    width, _ = window.dimensions()
    print(f"PASS {width:g}px no empty dock/strip; rail roots (including Saved offline) have no Back and preserve Search x/width",
          flush=True)


def playlist_back(window, *, wide):
    before = pane_layout(window, wide=wide)
    open_manual(window)
    detail = pane_layout(window, wide=wide, can_go_back=True)
    check(all(abs(a - b) <= 1 for a, b in zip(detail, before)),
          f"Back shifted Search: root {before}, detail {detail}")
    window.click("Back")
    playlists_root(window)
    print(f"PASS {window.dimensions()[0]:g}px playlist detail Back returns to the Playlists root", flush=True)


def playlists_root(window):
    window.section("Playlists")
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    window.absent("Back", roles=("Button",))


def playlist_hit_checks(window):
    for region in ("icon", "subtitle", "whitespace"):
        playlist_open_by_pointer(window, region)
    playlist_overflow(window)


def playlist_hit_targets(window):
    play = window.find(f"Play {PLAYLIST}", roles=("Button",))
    overflow = window.find(f"Show actions for {PLAYLIST}", roles=("Button",))
    px, py, pr, pb = bounds(play)
    ox, oy, oright, ob = bounds(overflow)
    check(px < pr <= ox < oright and py < pb and oy < ob, "playlist Play and overflow hit targets overlap")
    row = next((e for e in window.elements() if element_type([e], "MediaLine")
                and bounds(e)[0] <= px and bounds(e)[2] >= oright
                and bounds(e)[1] <= min(py, oy) < max(pb, ob) <= bounds(e)[3]), None)
    check(row, f"could not find the visible playlist row for {PLAYLIST}")
    elements = window.subtree(row)
    subtitle = next((e for e in elements if window.matches(e, r"Playlist · \d+ songs(?: · .+)?")), None)
    # The noninteractive separator sits inside the blank bottom padding, away
    # from the controls and the copy. It supplies an MCP pointer coordinate.
    return {"icon": element_type(elements, "Cover"), "subtitle": subtitle,
            "whitespace": element_type(elements, "Divider")}, play, overflow


def inside(point, element):
    x, y, right, bottom = bounds(element)
    return x <= point[0] < right and y <= point[1] < bottom


def playlist_open_by_pointer(window, region):
    playlists_root(window)
    targets, play, overflow = playlist_hit_targets(window)
    target = targets[region]
    check(target, f"missing {PLAYLIST} row {region} pointer target")
    point = window.pointer_position_for_click(region, element=target)
    check(inside(point, target) and not any(inside(point, e) for e in (play, overflow)),
          f"{region} press {point} did not hit its own part of the playlist row")
    manual_detail(window, timeout=2)
    window.absent(r"Now playing: .+", roles=("Button",))
    window.click("Back")
    playlists_root(window)
    print(f"PASS {window.dimensions()[0]:g}px playlist row {region} opens the correct detail by pointer", flush=True)


def playlist_overflow(window):
    playlists_root(window)
    _, play, overflow = playlist_hit_targets(window)
    point = window.pointer_position_for_click("playlist overflow", element=overflow)
    check(inside(point, overflow) and not inside(point, play), "playlist overflow press hit the Play target")
    window.find(f"Add to queue · {PLAYLIST}", roles=("Button",))
    playlists_root(window)
    window.absent(r"Now playing: .+", roles=("Button",))
    window.click(f"Hide actions for {PLAYLIST}")
    window.absent(f"Add to queue · {PLAYLIST}", roles=("Button",))
    print(f"PASS {window.dimensions()[0]:g}px playlist row overflow opens its menu without opening or playing the playlist",
          flush=True)


def playlist_play(window):
    _, play, overflow = playlist_hit_targets(window)
    check(play.get("accessibleEnabled", True), "playlist row Play is disabled")
    point = window.pointer_position_for_click("playlist Play", element=play)
    check(inside(point, play) and not inside(point, overflow), "playlist Play press hit the overflow target")


def empty_devices(window, restore, *, wide, observer=False, capture=None):
    window.click("Playback devices")
    window.title("Devices")
    if observer:
        deselect = window.find("Deselect", roles=("Button",))
        check(deselect.get("accessibleEnabled"), "the selected desktop output cannot be deselected")
        name = window.title("Acceptance observer")
        card = containing_item(window.elements(), "SettingCard", name)
        check(card, "missing Devices card for the real Go observer")
        elements = window.subtree(card)
        peer_button = next((e for e in elements if window.matches(e, "Select output")
                            and e.get("accessibleRole") == "Button"), None)
        check(peer_button and not peer_button.get("accessibleEnabled")
              and any(window.matches(e, "Cannot play audio") for e in elements),
              "real Go observer must be present and not selectable for audio")
    pane_layout(window, wide=wide, can_go_back=True)
    if capture:
        window.pixels(capture)
    window.click("Back")
    restore()
    window.absent("Back", roles=("Button",))
    print(f"PASS {window.dimensions()[0]:g}px device panel is reachable with no selected track", flush=True)


def queue_state(window, populated):
    window.title(QUEUE_TITLE)
    window.find("Close play queue", roles=("Button",))
    if populated:
        for label in QUEUE_CONTROLS:
            control = window.find(label, roles=("Button",))
            check(control.get("accessibleEnabled"), f"populated queue has disabled {label}")
        window.title("Now playing")
        window.title("Up next")
        window.find(r"Synthetic Tone \d{3}", roles=("Text",))
    else:
        window.find(r"The queue is empty\..*", roles=("Text",))
        for label in QUEUE_CONTROLS:
            window.absent(label, roles=("Button",))


def wide_queue_toggle(window, restore, *, populated=False, can_go_back=False):
    before = window.elements()
    catalog = element_id(before, "catalog-scroll")
    queue = element_id(before, "queue-pane")
    check(catalog and queue, "wide queue toggle requires both panes")
    window.click("Close play queue")
    restore()
    closed = window.elements()
    check(not element_id(closed, "queue-pane"), "desktop queue stays visible after Close play queue")
    resized = element_id(closed, "catalog-scroll")
    check(resized and abs(bounds(resized)[2] - bounds(queue)[2]) <= 2,
          "closing the desktop queue does not give its width back to the catalog")
    expanded = pane_layout(window, wide=True, docked=populated, queue_open=False, can_go_back=can_go_back)
    window.click("Play queue")
    restore()
    narrowed = pane_layout(window, wide=True, docked=populated, can_go_back=can_go_back)
    queue_left, _, queue_right, _ = bounds(queue)
    # Each pane uses its own responsive gutter on both sides. Account for
    # that change, not just the width released by the queue pane.
    gutter_delta = expanded[0] - narrowed[0]
    check(expanded[1] > narrowed[1]
          and abs(expanded[1] - narrowed[1] + 2 * gutter_delta - (queue_right - queue_left)) <= 2,
          f"Search and catalog do not expand and contract together: {expanded} vs {narrowed}")
    queue_state(window, populated)
    print("PASS desktop queue toggles independently and returns its width to Search and the catalog", flush=True)


def compact_queue(window, restore, *, capture=None, populated=False):
    window.click("Play queue")
    queue_state(window, populated)
    elements = window.elements()
    queue = element_id(elements, "queue-pane")
    work = element_id(elements, "work")
    toolbar = element_id(elements, "toolbar")
    check(queue and work and toolbar, "compact queue is missing its pane, workspace or toolbar")
    qx, qy, qr, qb = bounds(queue)
    tx, _, tr, tb = bounds(toolbar)
    check(abs(qx - tx) <= 1 and abs(qy - tb) <= 1 and abs(qr - tr) <= 1 and abs(qb - bounds(work)[3]) <= 1,
          f"compact queue does not fill the workspace below the toolbar: {bounds(queue)}")
    if capture:
        window.pixels(capture)
    window.click("Play queue")
    restore()
    window.absent("Shuffle", roles=("Button",))
    window.click("Play queue")
    window.title(QUEUE_TITLE)
    window.click("Close play queue")
    restore()
    window.absent("Shuffle", roles=("Button",))
    print("PASS compact queue opens and closes by pointer without losing the catalog route", flush=True)
