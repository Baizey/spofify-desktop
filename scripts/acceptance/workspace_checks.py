"""Pointer and layout contracts for the listening workspace's native window."""
import json
import re
from time import monotonic, sleep

from fixture import Failure, check
from native import (ALT, CONTROL, DESKTOP, ESCAPE, LEFT, OFFLINE_SWITCH, OLD_NAV, PLAYLIST,
                    RETURN, SAVED, TRACK, assert_disjoint, assert_dock_anchor, assert_empty_dock, bounds, contains,
                    dock_identity, element_id, element_type, named, player_bar, wait)
from wire_ui import (ALBUM_NAME, ARTIST_NAME, ENTRIES, GUEST_NAME, ID_ONLY_RELEASE,
                     LARGE_MANUAL, LARGE_PLAYLIST_COUNT, LARGE_RULE, NAME_ONLY,
                     NEGATIVE_PLAYLISTS, SINGLE_TRACK, SUMMARY)


QUEUE_TITLE = "Queue"
QUEUE_CONTROLS = ("Shuffle", r"Repeat: (off|one|all)")


def open_playlists(window):
    window.click("Playlists")
    window.section("Playlists")
    window.find(f"Open {PLAYLIST}", roles=("Button",))


def open_manual(window):
    window.click(f"Open {PLAYLIST}")
    manual_detail(window)


def play_manual(window):
    actions = element_id(window.elements(), "playlist-tools")
    check(actions, "missing playlist actions for Play")
    window.click("Play", within=actions)


def manual_detail(window, timeout=10):
    window.title(PLAYLIST, timeout=timeout)
    wait("two distinct playlist occurrences", lambda: sum(
        window.matches(element, f"Play track: {TRACK}") and element.get("accessibleRole") == "Button"
        for element in catalog_rows(window)) >= 2, timeout=timeout)


def playlist_option(action, title):
    return re.escape(f"{action} · {title}")


def playlist_option_trigger(title):
    return re.escape(f"Show actions for {title}")


EDITORS = {"Rename": ("Rename playlist", "Playlist name"),
           "Edit rules": ("Edit playlist rules", "Playlist rules definition"),
           "Delete": ("Delete this playlist?", None)}
DETAIL_ACTIONS = ("Play", "Add to queue", "Save offline", "Remove offline save",
                  "Remove from Saved", *EDITORS)


def row_offline_option(window, title, *, saved=False):
    action = "Remove offline save" if saved else "Save offline"
    label = playlist_option(action, title)
    item = window.find(label, roles=("Button",))
    check(item.get("accessibleCheckable") and item.get("accessibleChecked", False) == saved,
          f"wrong accessible offline state for {label}")
    window.absent(playlist_option("Save offline" if saved else "Remove offline save", title), roles=("Button",))
    return label


def detail_actions(window, expected, *, optional=()):
    def read():
        elements = window.elements()
        toolbar = element_id(elements, "playlist-tools")
        if toolbar is None:
            return None
        buttons = [e for e in window.subtree(toolbar) if e.get("accessibleRole") == "Button"]
        labels = [e.get("accessibleLabel") for e in buttons]
        return (elements, toolbar, buttons, labels) if all(label in labels for label in expected) else None

    elements, toolbar, buttons, labels = wait(f"direct detail actions {expected}", read)
    check(len(labels) == len(set(labels)) and set(labels) <= set(expected) | set(optional),
          f"unexpected or duplicate detail actions: {labels}")
    window.absent(r"Manage playlist|Hide playlist options", roles=("Button",))
    generic_tools = element_id(elements, "page-tools")
    if generic_tools:
        check(not any(e.get("accessibleRole") == "Button" for e in window.subtree(generic_tools)),
              "generic page tools duplicate the playlist detail actions")
    viewport = element_id(elements, "catalog-scroll")
    check(viewport, "missing playlist detail viewport")
    for label, button in zip(labels, buttons):
        x, y, right, bottom = bounds(button)
        check(right - x >= 36 and bottom - y >= 36 and contains(toolbar, button) and contains(viewport, button),
              f"detail action {label} is undersized or clipped: {bounds(button)}")
        if label in expected:
            check(button.get("accessibleEnabled", True), f"detail action {label} is disabled")
        if label == "Save offline" or (label == "Remove offline save" and "Rename" in expected):
            check(button.get("accessibleCheckable") and button.get("accessibleChecked", False) == (label != "Save offline"),
                  f"wrong accessible offline state for direct action {label}")
    assert_disjoint(list(zip(labels, buttons)))
    if "Play" in expected:
        x, y, _, _ = bounds(buttons[0])
        check(labels[0] == "Play" and all(top >= y - 1 and (top > y + 1 or left >= x - 1)
                                         for left, top, _, _ in map(bounds, buttons)),
              "detail Play is not the leading action")
    # The playback dock can have its own Play button. The other unsuffixed
    # labels belong to the detail toolbar, not to track rows or other tools.
    for label in DETAIL_ACTIONS[1:]:
        count = sum(window.matches(e, re.escape(label)) and e.get("accessibleRole") == "Button" for e in elements)
        check(count == labels.count(label), f"{label} is duplicated outside the detail actions")
    return dict(zip(labels, buttons))


def editor_actions(summary):
    caps = summary["capabilities"]
    return tuple(action for action, allowed in (("Rename", caps["editMetadata"]),
                 ("Edit rules", summary["kind"] == "derived" and caps["editDefinition"]),
                 ("Delete", caps["delete"])) if allowed)


def cancel_and_reopen_editor(window, title, action, activate):
    heading, field = EDITORS[action]
    dock = dock_identity(window)

    def opened():
        window.title(heading)
        check(dock_identity(window) == dock, f"opening {action} moved or replaced the dock")
        if field:
            item = window.find(field, roles=("TextInput",))
            if action == "Rename":
                check(item.get("accessibleValue") == title, "Rename opened for the wrong playlist")
        window.find("Cancel", roles=("Button",))

    activate()
    opened()  # Row editors open asynchronously after routing to detail.
    window.key(RETURN if action == "Delete" else ESCAPE)
    window.title(title)
    window.find(action, roles=("Button",))
    window.absent("Cancel", roles=("Button",))
    window.key(RETURN)  # Do not click or refocus the detail action.
    opened()
    window.key(ESCAPE)
    window.title(title)
    window.absent("Cancel", roles=("Button",))
    check(dock_identity(window) == dock, f"cancelling {action} moved or replaced the dock")


def manual_actions(window, *, capture=None):
    detail_actions(window, ("Play", "Add to queue", "Save offline", "Rename", "Delete"))
    if capture:
        window.pixels(capture)
    print(f"PASS {window.dimensions()[0]:g}px manual playlist has single direct actions", flush=True)


def playlist_case_actions(window, summary, activate=None, *, capture=None):
    title = summary["name"]
    label = re.escape(title)
    count = summary["trackCount"]
    ready = summary["kind"] == "manual" or summary.get("evaluationState") == "ready"
    playable = ready and summary["capabilities"]["play"] and summary["availableCount"] > 0
    row_play = f"Play {label}"
    window.find(f"Open {label}", roles=("Button",))
    if playable:
        window.find(row_play, roles=("Button",))
    else:
        window.absent(row_play, roles=("Button",))
    window.click(playlist_option_trigger(title))
    row_offline_option(window, title)
    row_actions = (*EDITORS, "Add to queue")
    expected_row = set(editor_actions(summary)) | ({"Add to queue"} if playable else set())
    for action in row_actions:
        pattern = playlist_option(action, title)
        if action in expected_row:
            window.find(pattern, roles=("Button",))
        else:
            window.absent(pattern, roles=("Button",))
    window.key(ESCAPE)
    if activate and playable:
        activate(lambda: window.click(row_play))

    window.click(f"Open {label}")
    window.title(title)
    if not count:
        window.title("No items here yet")
    # The direct Save action is also a readiness fence for committed detail.
    actions = detail_actions(window, (*(("Play", "Add to queue") if playable else ()),
                                      "Save offline", *editor_actions(summary)))
    if capture and window.output:
        window.pixels(capture, DESKTOP[0])
    if activate and playable:
        activate(lambda: window.click_element(actions["Play"]))
    window.click("Back")
    playlists_root(window)


def synthetic_playlist_actions(window, play_requests):
    open_playlists(window)
    expected = play_requests()
    for summary, _, _ in (*NEGATIVE_PLAYLISTS, LARGE_MANUAL, LARGE_RULE):
        kind = "derived" if summary["kind"] == "derived" else "playlist"
        selection = dict(kind="collection", context=dict(kind=kind, id=summary["id"], name=summary["name"]))

        def activate(click):
            check(play_requests() == expected, "playlist browsing sent an unexpected Play")
            click()
            expected.append(selection)
            observed = wait("synthetic collection Play", lambda: play_requests()
                            if len(play_requests()) >= len(expected) else None)
            check(observed == expected, "playlist Play used the wrong context, ID, or number of requests")

        playlist_case_actions(window, summary, activate,
                              capture="playlist-actions-wire-large-rules" if summary == LARGE_RULE[0] else None)
    sleep(0.6)  # Drain accidental asynchronous Play from the final navigation.
    check(play_requests() == expected, "playlist navigation sent an unexpected Play")
    print("PASS synthetic row/detail Play and exact context for 1,161-song manual/rule playlists; "
          "no Play for empty, no-audio, forbidden or stale cases; offline commands in ellipses", flush=True)


def seed_playlist_actions(api, track_id):
    """Seed only the runner-owned Go account through the supported write lane."""
    from playback import Peer

    with Peer(api, name="Disposable playlist action seeder") as writer:
        def create(name, source=None):
            fields = dict(name=f"Acceptance {name}")
            if source is not None:
                fields["definition"] = dict(version=1, sources=[dict(kind="playlist", playlistId=source["id"],
                                                                     slotId="s_" + "1".zfill(32))],
                                            duplicates="keep", sort="sourceOrder")
            writer.personal("createPlaylist", **fields)
            return writer.personal_change(lambda ch: ch.get("playlist", {}).get("name") == fields["name"])["playlist"]

        def ready_rule(rule):
            if rule.get("evaluationState") != "ready":
                rule = writer.personal_change(lambda ch: ch.get("playlist", {}).get("id") == rule["id"]
                                              and ch["playlist"].get("evaluationState") == "ready",
                                              timeout=30)["playlist"]
            return rule

        empty = create("Empty Manual")
        big = create("Large Manual")
        pid = big["id"]
        # One acknowledged change at a time avoids overrunning the Go account
        # job queue. Duplicates are real canonical occurrences, not fake counts.
        deadline = monotonic() + 120
        for count in range(1, LARGE_PLAYLIST_COUNT + 1):
            writer.personal("addEntry", playlistId=pid, trackId=track_id)
            change = writer.personal_change(lambda ch: ch.get("playlist", {}).get("id") == pid,
                                            timeout=max(0, min(8, deadline - monotonic())))
            big = change["playlist"]
            check(big["trackCount"] == count, "Go did not commit the next manual occurrence")
        cases = (empty, ready_rule(create("Empty Rules", empty)), big, ready_rule(create("Large Rules", big)))
        for summary, count in zip(cases, (0, 0, LARGE_PLAYLIST_COUNT, LARGE_PLAYLIST_COUNT)):
            detail = writer.playlist(summary["id"], timeout=30)
            check(detail["summary"] == summary and summary["trackCount"] == summary["availableCount"] == count
                  and len(detail["rows"]) == count, "canonical Go playlist action fixture did not match its case")
    return cases


def real_playlist_actions(window, api, ready):
    from playback import Peer

    window.click("History")  # Don't re-render the playlist list for each seed commit.
    window.section("History")
    cases = seed_playlist_actions(api, ready["duplicateTrackId"])
    open_playlists(window)
    for summary in cases:
        kind = "derived" if summary["kind"] == "derived" else "playlist"
        identity = "occurrenceId" if kind == "derived" else "entryId"

        def activate(click):
            with Peer(api, name="Acceptance playlist Play observer") as observer:
                click()
                frame = observer.wait_frame(lambda f: f.get("player", {}).get("queue", {}).get("kind") == "replace",
                                            timeout=20)
                delta = frame["player"]
                rows = delta["queue"]["rows"]
                check(delta["cursor"].get("desired") and len(rows) == LARGE_PLAYLIST_COUNT
                      and len({row.get(identity) for row in rows}) == LARGE_PLAYLIST_COUNT
                      and all(row.get(identity) and row.get("playbackContext", {}).get("id") == summary["id"]
                              and row["playbackContext"].get("kind") == kind for row in rows),
                      "Go Play did not use the complete canonical collection and its correct context")

        playlist_case_actions(window, summary, activate)
    print("PASS real Go 0/1,161 manual and rule playlists; both Play surfaces queue all occurrences with canonical context",
          flush=True)


def cancel_remove_confirmation(window, title, action="Remove offline save"):
    window.click(action)
    window.title("Remove this saved collection?")
    window.find("Remove from Saved", roles=("Button",))
    window.find("Cancel", roles=("Button",))
    window.key(RETURN)  # The default must cancel, never remove the pin.
    window.title(title)
    window.find(action, roles=("Button",))
    window.absent("Cancel", roles=("Button",))


def real_playlist_offline_actions(window, ready, *, offline_mode):
    # Use the smaller stock rule playlist for a bounded sync. Check durable
    # intent in both menus, not completion of the background media transfer.
    open_playlists(window)
    target = next(t for t in ready["targets"] if t.get("playlistKind") == "derived")
    name = target["name"]
    label = re.escape(name)
    window.click(f"Open {label}")
    window.title(name)
    detail_actions(window, ("Play", "Add to queue", "Save offline", "Rename", "Edit rules", "Delete"))
    window.click("Save offline")

    def visit_saved():
        def open_section():
            elements = window.elements()
            current = any(window.matches(e, SAVED) and (e.get("accessibleDescription") or "").startswith("Current section")
                          for e in elements)
            on_detail = any(window.matches(e, "Back") and e.get("accessibleRole") == "Button" for e in elements)
            if current and not on_detail:
                return True
            # Navigation presses during the durable write are ignored.
            window.click(SAVED)
            return False

        wait("Saved offline listing after durable playlist change", open_section)

    def work_offline():
        window.click("Settings")
        window.section("Settings")
        window.click("Work offline")
        window.find("Go online", roles=("Button",))
        window.section("Settings")

    def saved_detail_actions(remove_action, *, offline):
        buttons = detail_actions(window, (remove_action,), optional=("Play", "Add to queue"))
        playback = {label for label in ("Play", "Add to queue") if label in buttons}
        check(not playback or playback == {"Play", "Add to queue"},
              f"Saved detail exposes only one local playback action: {playback}")
        check(all(buttons[label].get("accessibleEnabled", True) for label in playback),
              "Saved detail has a disabled local playback action")
        if not offline or not playback:
            return  # Pending pins need not have usable local audio; online rows need no local menu.
        # A real downloadable occurrence has the local queue option. Exercise
        # its menu without queuing or mutating the fixture during removal checks.
        rows = window.elements()
        viewport = element_id(rows, "catalog-scroll")
        face = next((row for row in rows if row.get("accessibleRole") == "Button"
                     and (row.get("accessibleLabel") or "").startswith("Play track: ")
                     and viewport and contains(viewport, row)), None)
        check(face, "expected a visible playable Saved occurrence")
        track = face["accessibleLabel"].removeprefix("Play track: ")
        window.click(playlist_option_trigger(track))
        window.find(playlist_option("Add to queue", track), roles=("Button",))
        window.absent(rf"(?:Add to playlist|Save offline|Remove|Delete) · {re.escape(track)}", roles=("Button",))
        window.key(ESCAPE)
        window.absent(playlist_option("Add to queue", track), roles=("Button",))

    def row_toggle(saved):
        window.click(playlist_option_trigger(name))
        row_offline_option(window, name, saved=saved)
        if saved and window.output:
            window.pixels("playlist-actions-real-row-saved")
        window.key(ESCAPE)

    def detail_toggle(saved):
        window.click(f"Open {label}")
        window.title(name)
        detail_actions(window, ("Play", "Add to queue", "Remove offline save" if saved else "Save offline",
                                "Rename", "Edit rules", "Delete"))
        if not saved and window.output:
            window.pixels("playlist-actions-real-detail-unsaved")

    visit_saved()
    window.find(f"Open {label}", roles=("Button",))
    window.click(f"Open {label}")
    window.title(name)
    saved_detail_actions("Remove offline save", offline=False)
    print("PASS playlist Save created durable intent visible in Saved offline and its online detail", flush=True)
    open_playlists(window)
    row_toggle(saved=True)
    detail_toggle(saved=True)
    cancel_remove_confirmation(window, name)
    detail_actions(window, ("Play", "Add to queue", "Remove offline save", "Rename", "Edit rules", "Delete"))
    print("PASS online row reflects durable pin; direct detail removal asks for confirmation with Cancel first", flush=True)

    work_offline()
    visit_saved()
    window.resize(offline_mode)
    check(window.dimensions()[0] == offline_mode[0], f"expected {offline_mode[0]} logical px for Saved detail")
    window.click(f"Show actions for {label}")
    row_offline_option(window, name, saved=True)
    window.key(ESCAPE)
    window.click(f"Open {label}")
    window.title(name)
    saved_detail_actions("Remove offline save", offline=True)
    print("PASS explicit Work offline keeps the checked Saved row action and direct detail removal", flush=True)
    if window.output:
        window.pixels("playlist-actions-real-offline-detail-saved")
    cancel_remove_confirmation(window, name)
    window.key(RETURN)  # Reopen from the restored direct removal action.
    window.title("Remove this saved collection?")
    window.click("Remove from Saved")
    window.title(SAVED)  # A successful removal returns to the listing itself.
    window.section(SAVED)
    window.absent("Back", roles=("Button",))

    def only_independent_save_remains():
        buttons = [e for e in window.elements() if e.get("accessibleRole") == "Button"]
        return (any(window.matches(e, f"Play track: {TRACK}") for e in buttons)
                and not any(window.matches(e, f"Open {label}") for e in buttons))

    wait("offline listing drops playlist but keeps the independently saved track", only_independent_save_remains)
    print("PASS offline detail unpin removed the playlist and retained the independent track", flush=True)
    window.click("Settings")
    window.section("Settings")
    window.click("Go online")
    window.find("Work offline", roles=("Button",))
    window.section("Settings")
    open_playlists(window)
    row_toggle(saved=False)
    detail_toggle(saved=False)
    print("PASS real Go fixture: online row/detail reflect durable Save; offline detail removes only the playlist; "
          "both online toggles return to Save offline", flush=True)

    # The Go fixture has no legacy saved playlist. Save its stock album through
    # the UI to reach the same legacy Saved Collection detail/remove path.
    album = next(t["name"] for t in ready["targets"] if t["kind"] == "album")
    album_label = re.escape(album)
    open_playlists(window)
    open_manual(window)
    window.click(f"Open album: {album_label}")
    window.title(album)
    window.click("Save offline")
    window.find(r"Saved (?:all \d+ tracks on this device\.|\d+ of \d+ tracks\. Some audio is unavailable\.)",
                roles=("Text",), timeout=60)
    visit_saved()
    window.click(f"Open {album_label}")
    window.title(album)
    saved_detail_actions("Remove from Saved", offline=False)
    work_offline()
    visit_saved()
    window.click(f"Open {album_label}")
    window.title(album)
    saved_detail_actions("Remove from Saved", offline=True)
    cancel_remove_confirmation(window, album, "Remove from Saved")
    window.key(RETURN)
    window.title("Remove this saved collection?")
    window.click("Remove from Saved")
    window.title(SAVED)
    window.section(SAVED)
    window.absent("Back", roles=("Button",))
    window.find(f"Play track: {TRACK}", roles=("Button",))
    window.absent(f"Open {album_label}", roles=("Button",))
    print("PASS legacy Saved detail confirms removal and retains the independently saved track", flush=True)


def playlist_row_menu(window):
    add = f"Add to playlist · {TRACK}"
    window.absent(add, roles=("Button",))
    window.click(f"Show actions for {TRACK}")
    window.find(f"Remove · {TRACK}", timeout=3, roles=("Button",))
    window.click(add)
    window.title("Add to playlist")
    window.find(f"Add to {PLAYLIST}", roles=("Button",))
    window.click("Back")
    window.title(PLAYLIST)
    window.absent(add, roles=("Button",))
    print("PASS track overflow activates the playlist picker and is dismissed on Back", flush=True)


def rules_editor(window, capture=None):
    dock = dock_identity(window)
    window.click("New rules playlist")
    window.title("New rules playlist")
    check(dock_identity(window) == dock, "opening the new rules editor moved or replaced the dock")
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
    check(dock_identity(window) == dock, "cancelling the new rules editor moved or replaced the dock")
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
    for title in (ARTIST_NAME, ALBUM_NAME):
        window.find(f"Open {title}", roles=("Button",))
    track_row(window)
    window.absent(f"Open {TRACK}", roles=("Button",))
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


def catalog_rows(window):
    listing = element_id(window.elements(), "catalog-list")
    check(listing, "missing catalog row list")
    # Scope to this subtree even when the same track is playable in the queue.
    # The large Go fixture may fill MCP's cap; callers check the fixture's
    # first occurrences rather than treating the returned rows as exhaustive.
    return window.mcp.value("get_element_tree", {
        "elementHandle": listing["handle"], "maxElements": 1000})["elements"]


def track_row(window, title=TRACK, occurrence=0):
    def body():
        catalog = catalog_rows(window)
        matches = sorted((e for e in catalog if e.get("accessibleRole") == "Button"
                          and window.matches(e, f"Play track: {re.escape(title)}")), key=lambda e: bounds(e)[1])
        return (catalog, matches[occurrence]) if occurrence < len(matches) else None

    catalog, target = wait(f"catalog track {title}, occurrence {occurrence + 1}", body)
    row = containing_item(catalog, "MediaLine", target)
    check(row, "missing rendered MediaLine for track hit testing")
    return row, window.subtree(row)


def track_face(window, region="title", *, title=TRACK, occurrence=0, double=False):
    row, elements = track_row(window, title, occurrence)
    # The accessible row button covers the face; its center could coincide
    # with an independent credit/control. Hit title or padding deliberately.
    words = element_id(elements, "words")
    check(words, "missing track title layout")
    target = (element_type(window.subtree(words), "Text") if region == "title"
              else element_type(elements, "Divider"))
    controls = [e for e in elements if e.get("accessibleRole") == "Button"
                and not window.matches(e, r"Play track: .+")]
    check(target, f"missing track {region} pointer target")
    viewport = element_id(window.elements(), "catalog-scroll")
    x, y, right, bottom = bounds(target)
    check(viewport and inside(((x + right) / 2, (y + bottom) / 2), viewport), "track hit target is outside the catalog viewport")
    point = window.pointer_position_for_click(f"track {region}", element=target, double=double)
    check(inside(point, target) and inside(point, row) and not any(inside(point, e) for e in controls),
          f"track {region} press {point} landed on a control or outside its row")


def search_track_targets(window, width, capture, *, check_padding):
    track_face(window)
    search_results(window)
    if check_padding:
        track_face(window, "bottom padding")
        search_results(window)
    for name in ((ARTIST_NAME, ALBUM_NAME) if check_padding else (ARTIST_NAME, GUEST_NAME, ALBUM_NAME)):
        is_album = name == ALBUM_NAME
        link = f"Open {'album' if is_album else 'artist'}: {name}"
        row, elements = track_row(window)
        target = next((e for e in elements if e.get("accessibleRole") == "Button"
                       and window.matches(e, re.escape(link))), None)
        check(target, f"missing search track link {link}")
        point = window.pointer_position_for_click(link, element=target)
        check(inside(point, target) and inside(point, row), f"{link} press did not hit the track row link")
        if is_album:
            window.title(ALBUM_NAME)
            window.find(f"Play track: {TRACK}", roles=("Button",))
            qualified_credits(window)
            if capture:
                window.pixels(f"album-{width}", width)
            window.click(f"Open artist: {ARTIST_NAME}")
            artist(window, ARTIST_NAME)
            window.click("Back")
            window.title(ALBUM_NAME)
        else:
            artist(window, name)
        window.click("Back")
        search_results(window)
    print("PASS search track title/padding stay on Results; in-row artist and album links open only their destinations",
          flush=True)


def track_play_and_highlight(window, play_requests):
    # The synthetic peer observes commands and publishes a paused cursor from
    # those selections; it does not resolve media or execute playback.
    open_playlists(window)
    open_manual(window)
    check(not play_requests(), "browsing, credits, or overflow sent an unexpected Play")
    track_face(window, occurrence=1)
    sleep(0.6)  # Drain any accidental async Play and end the single-click sequence.
    window.title(PLAYLIST)
    check(not play_requests(), "single track click sent Play")

    entry = ENTRIES[1]
    selected = dict(kind="track", track=dict(id=entry["track"]["id"], entryId=entry["id"]),
                    context=dict(kind="playlist", id=SUMMARY["id"], name=PLAYLIST))
    expected = []

    def expect(selection):
        expected.append(selection)
        check(wait("native Play request", lambda: play_requests() if len(play_requests()) >= len(expected) else None)
              == expected, "native activation sent duplicate or wrong occurrence Play")

    track_face(window, occurrence=1, double=True)
    expect(selected)
    window.title(PLAYLIST)
    _, elements = track_row(window, occurrence=1)
    explicit = next((e for e in elements if window.matches(e, f"Play {TRACK}")), None)
    check(explicit, "missing explicit track Play button")
    window.click_element(explicit)
    expect(selected)
    sleep(0.6)
    _, elements = track_row(window, occurrence=1)
    explicit = next((e for e in elements if window.matches(e, f"Play {TRACK}")), None)
    check(explicit, "missing explicit track Play button after selection")
    window.pointer_position_for_click("explicit track Play", element=explicit, double=True)
    expect(selected)
    track_face(window, occurrence=1)
    window.key(RETURN)  # Only the focused row face can send this exact entry.
    expect(selected)
    window.title(PLAYLIST)

    # Keep both search rows visible above the dock when clicking the second.
    if window.window_properties()["size"]["width"] != DESKTOP[0]:
        window.resize(DESKTOP)
    window.fill("Search tracks, albums or artists", "Synthetic")
    search_results(window)
    first = f"Play track: {TRACK}"
    other = f"Play track: {re.escape(SINGLE_TRACK['name'])}"

    def state(pattern, description):
        return next((e for e in catalog_rows(window) if window.matches(e, pattern)
                     and (e.get("accessibleDescription") or "") == description), None)

    a = wait("first current recording in the new catalog", lambda: state(first, "Current track"))
    b = wait("other track without a marker", lambda: state(other, ""))
    _, elements = track_row(window, title=SINGLE_TRACK["name"])
    explicit = next((e for e in elements if window.matches(e, f"Play {re.escape(SINGLE_TRACK['name'])}")), None)
    check(explicit, "missing other recording's Play button")
    window.click_element(explicit)
    expect(dict(kind="track", track=dict(id=SINGLE_TRACK["id"])))
    a_after = wait("old current track clears", lambda: state(first, ""))
    b_after = wait("current marker moves to the new recording", lambda: state(other, "Current track"))
    check(bounds(a) == bounds(a_after) and bounds(b) == bounds(b_after), "highlight moved track hit targets")
    search_results(window)
    sleep(0.6)
    check(play_requests() == expected, "a gesture or metadata link sent more than one Play")
    print("PASS single click focuses; face double-click, explicit Play (including double-click) and Return select the same occurrence once; highlight follows recording ID",
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
    window.find(f"Play track: {TRACK}", roles=("Button",))
    window.click("Back")
    search_results(window)

    search_track_targets(window, width, capture, check_padding=check_compact_targets)
    open_playlists(window)
    window.absent("Back", roles=("Button",))
    print(f"PASS at {width}px: typed search, ID-only release, named credit routes and Back", flush=True)


def check_shortcut_back(window):
    open_manual(window)
    window.shortcut(ALT, LEFT)
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    window.absent("Back", roles=("Button",))


def search_layout(window, elements, toolbar):
    # TextInput excludes the icon and clear affordance; measure the painted Field.
    field = element_id(elements, "search")
    header = element_id(elements, "header-slot")
    check(field and header, "missing rendered Search field or catalog content inset")
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
    content_left, _, content_right, _ = bounds(header)
    inset = x - tx
    check(0 < inset <= 40 and abs(x - content_left) <= 2,
          f"Search is not aligned with the catalog gutter: {x:g} vs {content_left:g}")
    usable_right = tright - inset
    check(abs(content_right - usable_right) <= 2,
          f"catalog content does not fill its usable width: ends at {content_right:g}, expected {usable_right:g}")
    check(abs(right - usable_right) <= 2,
          f"Search leaves unused toolbar space: field ends at {right:g}, usable edge at {usable_right:g}")
    return x, right - x


def pane_layout(window, *, wide, queue_open=None, can_go_back=False):
    elements = window.elements()
    work = element_id(elements, "work")
    rail = element_id(elements, "rail")
    toolbar = element_id(elements, "toolbar")
    catalog = element_id(elements, "catalog-scroll")
    queue = element_id(elements, "queue-pane")
    dock = assert_dock_anchor(window, elements)
    check(work and rail and toolbar and catalog, "missing work area, navigation rail, toolbar or catalog viewport")
    width, height = window.dimensions()
    x, y, right, bottom = bounds(catalog)
    check(0 <= x < right <= width and 0 <= y < bottom <= height, "catalog viewport leaves client window")
    wx, wy, wright, wbottom = bounds(work)
    rx, ry, rright, rbottom = bounds(rail)
    check(wx == rx == wy == ry == 0 and wright == width
          and abs(rright - x) <= 2 and abs(rbottom - bottom) <= 2 and abs(wbottom - bottom) <= 2,
          f"navigation rail and workspace do not line up: {bounds(rail)}, {bounds(work)}, {bounds(catalog)}")
    check(abs(bottom - bounds(dock)[1]) <= 1, "catalog viewport does not meet the dock")
    tx, ty, tright, tbottom = bounds(toolbar)
    check(ty == 0 and abs(tx - x) <= 1 and abs(tright - right) <= 1,
          f"toolbar and catalog have different edges: {bounds(toolbar)}, {bounds(catalog)}")
    search = search_layout(window, elements, toolbar)
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
    dock = assert_empty_dock(window)
    # Exercise the actual rail, including both ways away from Saved offline.
    for label in ("Playlists", "Saved offline", "History", "Saved offline", "Settings", "Playlists"):
        window.click(label)
        window.section(label)
        actual_search = pane_layout(window, wide=wide)
        check(dock_identity(window) == dock, f"root {label} moved or replaced the empty dock")
        check(all(abs(a - b) <= 1 for a, b in zip(actual_search, expected_search)),
              f"Search x/width moved on root {label}: {actual_search} vs {expected_search}")
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    width, _ = window.dimensions()
    print(f"PASS {width:g}px rail roots preserve the empty dock and Search x/width without adding Back",
          flush=True)


def playlist_back(window, *, wide):
    before = pane_layout(window, wide=wide)
    dock = dock_identity(window)
    open_manual(window)
    detail = pane_layout(window, wide=wide, can_go_back=True)
    check(dock_identity(window) == dock, "opening the playlist detail moved or replaced the dock")
    check(all(abs(a - b) <= 1 for a, b in zip(detail, before)),
          f"Back shifted Search: root {before}, detail {detail}")
    window.click("Back")
    playlists_root(window)
    check(dock_identity(window) == dock, "returning from playlist detail moved or replaced the dock")
    print(f"PASS {window.dimensions()[0]:g}px playlist detail Back returns to the Playlists root", flush=True)


def playlists_root(window):
    window.section("Playlists")
    window.find(f"Open {PLAYLIST}", roles=("Button",))
    window.absent("Back", roles=("Button",))


def playlist_hit_checks(window):
    for region in ("subtitle", "whitespace"):
        playlist_open_by_pointer(window, region)
    playlist_overflow(window)


def playlist_editor_actions(window, rule_title):
    for title, actions in ((PLAYLIST, ("Rename", "Delete")), (rule_title, ("Edit rules",))):
        for action in actions:
            open_playlists(window)
            window.click(playlist_option_trigger(title))
            cancel_and_reopen_editor(window, title, action, lambda: window.click(playlist_option(action, title)))
            window.click("Back")
            playlists_root(window)
        window.click(re.escape(f"Open {title}"))
        window.title(title)
        if title == PLAYLIST:
            manual_actions(window)
        else:
            detail_actions(window, ("Save offline", "Rename", "Edit rules", "Delete"))
        for action in actions:
            cancel_and_reopen_editor(window, title, action, lambda: window.click(action))
        window.click("Back")
        playlists_root(window)
    print(f"PASS {window.dimensions()[0]:g}px row and direct editors open, cancel and restore the detail action",
          flush=True)


def playlist_hit_targets(window):
    play = window.find(f"Play {PLAYLIST}", roles=("Button",))
    overflow = window.find(playlist_option_trigger(PLAYLIST), roles=("Button",))
    px, py, pr, pb = bounds(play)
    ox, oy, oright, ob = bounds(overflow)
    check(px < pr <= ox < oright and py < pb and oy < ob, "playlist Play and overflow hit targets overlap")
    row = next((e for e in window.elements() if element_type([e], "MediaLine")
                and bounds(e)[0] <= px and bounds(e)[2] >= oright
                and bounds(e)[1] <= min(py, oy) < max(pb, ob) <= bounds(e)[3]), None)
    check(row, f"could not find the visible playlist row for {PLAYLIST}")
    elements = window.subtree(row)
    cover = element_type(elements, "Cover")
    if cover:
        # Play now owns the cover hit target; playlist_case_actions separately
        # verifies that this pointer action sends the exact collection Play.
        check(all(abs(a - b) <= 1 for a, b in zip(bounds(play), bounds(cover))),
              "playlist Play is not centered on the cover")
    subtitle = next((e for e in elements if window.matches(e, r"Playlist · \d+ songs(?: · .+)?")), None)
    # The noninteractive separator sits inside blank bottom padding, away
    # from the controls and the copy. It supplies an MCP pointer coordinate.
    return {"subtitle": subtitle, "whitespace": element_type(elements, "Divider")}, play, overflow


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
    save = row_offline_option(window, PLAYLIST)
    width, height = window.dimensions()
    for action in ("Add to queue", "Save offline", "Rename", "Delete"):
        label = playlist_option(action, PLAYLIST)
        button = window.find(label, roles=("Button",))
        x, y, right, bottom = bounds(button)
        check(0 <= x < right <= width + 1 and 0 <= y < bottom <= height + 1,
              f"playlist row action {label} is clipped: {bounds(button)}")
    window.absent(playlist_option("Edit rules", PLAYLIST), roles=("Button",))
    window.key(ESCAPE)
    playlists_root(window)
    window.absent(save, roles=("Button",))
    window.key(RETURN)
    window.find(save, roles=("Button",))
    window.key(ESCAPE)
    window.find(playlist_option_trigger(PLAYLIST), roles=("Button",))
    window.absent(save, roles=("Button",))
    window.absent(r"Now playing: .+", roles=("Button",))
    print(f"PASS {window.dimensions()[0]:g}px playlist row overflow offers Save offline and restores focus on Escape",
          flush=True)


def empty_devices(window, restore, *, wide, observer=False, capture=None):
    dock = assert_empty_dock(window)
    switch = window.find("Playback devices", roles=("Button",), within=player_bar(window))
    window.click_element(switch)
    window.title("Playback devices")
    window.absent("Back|Manage devices|Deselect", roles=("Button",))
    restore()
    if observer:
        selected = next((e for e in window.elements() if e.get("accessibleDescription") == "Selected output"), None)
        check(selected, "real Go output is not shown as selected")
        rename = window.find("Rename device: " + re.escape(selected["accessibleLabel"]), roles=("Button",))
        check(rename.get("accessibleEnabled"), "the selected desktop output must still offer rename")
        peer = window.find("Select output: Acceptance observer", roles=("Button",))
        check(not peer.get("accessibleEnabled") and peer.get("accessibleDescription") == "Cannot play audio",
              "real Go observer must be present and not selectable for audio")
    pane_layout(window, wide=wide)
    if capture:
        window.pixels(capture)
    window.key(ESCAPE)
    window.absent("Close playback devices", roles=("Button",))
    window.key(RETURN)
    window.title("Playback devices")
    window.click("Close playback devices")
    restore()
    window.absent("Back", roles=("Button",))
    check(dock_identity(window) == dock, "opening and dismissing devices moved or replaced the empty dock")
    print(f"PASS {window.dimensions()[0]:g}px empty-dock device popup preserves page and focus", flush=True)


def queue_state(window, populated):
    window.title(QUEUE_TITLE)
    pane = element_id(window.elements(), "queue-pane")
    check(pane, "missing queue pane")
    window.find("Close play queue", roles=("Button",), within=pane)
    if populated:
        for label in QUEUE_CONTROLS:
            control = window.find(label, roles=("Button",), within=pane)
            check(control.get("accessibleEnabled"), f"populated queue has disabled {label}")
        window.title("Now playing")
        window.find("Up next|Played", roles=("Text",))
        window.find(r"Synthetic Tone \d{3}", roles=("Text",))
    else:
        window.find(r"The queue is empty\..*", roles=("Text",), within=pane)
        elements = window.subtree(pane, max_elements=1000)
        for label in QUEUE_CONTROLS:
            check(not named(window, elements, label), f"empty queue pane exposes {label}")


def wide_queue_toggle(window, restore, *, populated=False, can_go_back=False):
    before = window.elements()
    dock = dock_identity(window, before)
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
    expanded = pane_layout(window, wide=True, queue_open=False, can_go_back=can_go_back)
    check(dock_identity(window) == dock, "closing the wide queue moved or replaced the dock")
    window.click("Play queue", within=player_bar(window))
    restore()
    narrowed = pane_layout(window, wide=True, can_go_back=can_go_back)
    check(dock_identity(window) == dock, "restoring the wide queue moved or replaced the dock")
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
    dock = dock_identity(window)
    window.click("Play queue", within=player_bar(window))
    queue_state(window, populated)
    elements = window.elements()
    check(dock_identity(window, elements) == dock, "opening the compact queue moved or replaced the dock")
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
    window.click("Play queue", within=player_bar(window))
    restore()
    window.absent("Close play queue", roles=("Button",))
    window.click("Play queue", within=player_bar(window))
    window.title(QUEUE_TITLE)
    window.click("Close play queue")
    restore()
    window.absent("Close play queue", roles=("Button",))
    check(dock_identity(window) == dock, "closing the compact queue moved or replaced the dock")
    print("PASS compact queue opens and closes by pointer without losing the catalog route", flush=True)
