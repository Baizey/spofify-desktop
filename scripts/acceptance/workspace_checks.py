"""Pointer and layout contracts for the listening workspace's native window."""
import json
import re

from fixture import Failure, check
from native import (ALT, CONTROL, DOWN, ESCAPE, LEFT, OFFLINE_SWITCH, OLD_NAV, PLAYLIST,
                    RETURN, TRACK, bounds, element_id, wait)
from wire_ui import ALBUM_NAME, ARTIST_NAME, GUEST_NAME, ID_ONLY_RELEASE, NAME_ONLY


QUEUE_TITLE = "Queue"
QUEUE_CONTROLS = ("Shuffle", r"Repeat: (off|one|all)")


def open_playlists(window):
    window.click("Playlists")
    window.section("Playlists")
    window.find(f"Open {PLAYLIST}", roles=("Button",))


def open_manual(window):
    window.click(f"Open {PLAYLIST}")
    window.title(PLAYLIST)
    wait("two distinct playlist occurrences", lambda: sum(
        window.matches(element, f"Open {TRACK}") and element.get("accessibleRole") == "Button"
        for element in window.elements()) >= 2)


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


def typed_catalog(window, width, *, capture=False, via_shortcut=False):
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
    # Shortcut has its own assertion; a failed shortcut never gets a mouse fallback.
    window.key(ESCAPE)
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    print(f"PASS at {width}px: typed search, ID-only release, named credit routes and Back", flush=True)


def check_shortcut_back(window):
    open_manual(window)
    window.shortcut(ALT, LEFT)
    window.find(f"Open {PLAYLIST}", roles=("Button",))


def pane_layout(window, *, wide, populated=False, can_go_back=False):
    elements = window.elements()
    work = element_id(elements, "work")
    rail = element_id(elements, "rail")
    toolbar = element_id(elements, "toolbar")
    catalog = element_id(elements, "catalog-scroll")
    queue = element_id(elements, "queue-pane")
    check(work and rail and toolbar and catalog, "missing work area, navigation rail, toolbar or catalog viewport")
    width, height = window.dimensions()
    x, y, right, bottom = bounds(catalog)
    check(0 <= x < right <= width and 0 <= y < bottom <= height, "catalog viewport leaves client window")
    wx, wy, wright, wbottom = bounds(work)
    rx, ry, rright, rbottom = bounds(rail)
    check(wx == rx == wy == ry == 0 and wright == width
          and abs(rright - x) <= 2 and abs(rbottom - bottom) <= 2 and abs(wbottom - bottom) <= 2,
          f"navigation rail and workspace do not line up: {bounds(rail)}, {bounds(work)}, {bounds(catalog)}")
    tx, ty, tright, tbottom = bounds(toolbar)
    check(ty == 0 and abs(tbottom - y) <= 1 and abs(tx - x) <= 1 and abs(tright - right) <= 1,
          f"toolbar overlaps the catalog or floats below the top: {bounds(toolbar)}, {bounds(catalog)}")
    targets = [window.find("Search tracks, albums or artists", roles=("TextInput",))]
    if can_go_back:
        targets.append(window.find("Back", roles=("Button",)))
    brand = window.title("Spofify")
    for element in targets:
        left, top, edge, base = bounds(element)
        check(tx <= left < edge <= tright and ty <= top < base <= tbottom,
              f"{element['accessibleLabel']} leaves the top toolbar: {bounds(element)}")
        check(abs((top + base - bounds(brand)[1] - bounds(brand)[3]) / 2) <= 10,
              f"{element['accessibleLabel']} is not aligned with the rail brand")
    if not wide:
        check(not queue, "compact window showed the queue without a request")
        check(right - x >= 0.72 * width, "compact workspace squeezes the catalog")
        return
    check(queue, "desktop is missing its persistent queue pane")
    qx, qy, qright, qbottom = bounds(queue)
    check(right <= qx < qright <= width and bottom <= height and qbottom <= height,
          f"overlapping or clipped catalog/queue: {bounds(catalog)}, {bounds(queue)}")
    check(qy == wy and 0 < y - qy <= 80 and abs(qbottom - bottom) <= 2,
          "desktop panes do not reach the same dock (with catalog below its toolbar)")
    check(right - x >= 0.55 * width and 245 <= qright - qx <= 0.35 * width,
          f"unusable pane widths: catalog {right - x:g}, queue {qright - qx:g}, window {width:g}")
    check(width - qright <= 32 and qx - right <= 24 and x <= 0.2 * width,
          "desktop wastes substantial space outside its panes")
    queue_state(window, populated)
    print(f"PASS desktop pane geometry at {width:g}×{height:g}", flush=True)


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


def wide_queue_toggle(window, restore):
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
    window.click("Play queue")
    restore()
    pane_layout(window, wide=True, can_go_back=True)
    print("PASS desktop queue toggles independently and returns its width to the catalog", flush=True)


def compact_queue(window, restore, *, capture=None, populated=False):
    window.click("Play queue")
    queue_state(window, populated)
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
