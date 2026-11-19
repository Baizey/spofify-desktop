"""Drive the published Slint MCP against a real, isolated Wayland window."""
from base64 import b64decode
from contextlib import contextmanager
import hashlib
from importlib.util import module_from_spec, spec_from_file_location
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import struct
import subprocess
from threading import Event, Thread
from time import monotonic, sleep
from urllib.error import URLError

from fixture import Failure, check, stop
from playback import Peer


DESKTOP = (1910, 1537)  # Cage's 35px client decoration leaves 1910×1502 content.
WIDE = (1280, 720)
COMPACT = (640, 640)
DOCK_TITLE = r"Now playing: Synthetic Tone \d{3}"
TRACK = "Synthetic Tone 001"
PLAYLIST = "Synthetic Manual Duplicates"
OFFLINE_SWITCH = r"Work offline|Go online|Stay offline"
OLD_NAV = r"Liked|Like|Unlike|Home|Search|Albums|Artists|Downloads"
SAVED = "Saved offline"
ESCAPE, CONTROL, ALT, LEFT, DOWN, RETURN = "\x1b", "\x11", "\x12", "\uf702", "\uf701", "\n"


def mcp_class():
    # Reuse the repository's upstream Slint MCP transport, not an app mock.
    helper = Path(__file__).resolve().parents[1] / "inspect-native.py"
    spec = spec_from_file_location("inspect_native", helper)
    module = module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.Mcp


def wait(label, fn, timeout=10):
    end = monotonic() + timeout
    while monotonic() < end:
        result = fn()
        if result:
            return result
        sleep(0.15)
    raise Failure(f"timed out waiting for {label}")


class Window:
    def __init__(self, mcp, handle, output, resize_output=None):
        self.mcp, self.handle, self.output = mcp, handle, output
        self.resize_output = resize_output
        self.properties = self.window_properties()
        self.pictures = set()

    def window_properties(self):
        return self.mcp.value("get_window_properties", {"windowHandle": self.handle})

    def resize(self, mode):
        before = self.window_properties()["size"]
        self.resize_output(mode)
        def resized():
            size = self.window_properties()["size"]
            return size if size["width"] == mode[0] and size != before else None
        return wait(f"same window resized to {mode[0]} physical px (was {before})", resized)

    def elements(self):
        tree = self.mcp.value("get_element_tree", {
            "elementHandle": self.properties["rootElementHandle"], "maxElements": 1000})
        check(not tree.get("debugInfoMissing"), "desktop requires SLINT_EMIT_DEBUG_INFO=1 at build time")
        elements = tree["elements"]
        # The catalog and queue can consume the tree budget before or halfway
        # through the dock. A returned player root doesn't prove the transport
        # was returned, so replace any partial dock with its complete subtree.
        if tree["truncated"]:
            player = element_id(elements, "player")
            handles = [player["handle"]] if player else self.mcp.value("find_elements_by_id", {
                "windowHandle": self.handle, "elementsId": "AppWindow::player"})["elementHandles"]
            check(len(handles) <= 1, "unexpected duplicate playback dock")
            for handle in handles:
                dock = self.subtree({"handle": handle})
                def handle_key(element):
                    h = element["handle"]
                    return h["index"], h["generation"]
                dock_keys = {handle_key(e) for e in dock}
                elements = [e for e in elements if handle_key(e) not in dock_keys] + dock
        return elements

    def subtree(self, element):
        tree = self.mcp.value("get_element_tree", {"elementHandle": element["handle"], "maxElements": 256})
        check(not tree["truncated"], "truncated native subtree inspection")
        return tree["elements"]

    @staticmethod
    def matches(element, pattern):
        return re.fullmatch(pattern, element.get("accessibleLabel") or "", re.IGNORECASE) is not None

    def find(self, pattern, timeout=10, roles=None):
        def matches():
            return next((element for element in self.elements() if self.matches(element, pattern)
                         and (roles is None or element.get("accessibleRole") in roles)), None)
        return wait(pattern, matches, timeout)

    def absent(self, pattern, roles=None):
        check(not any(self.matches(e, pattern) and (roles is None or e.get("accessibleRole") in roles)
                      for e in self.elements()), f"unexpected native element: {pattern}")

    def section(self, label):
        def current():
            return next((e for e in self.elements() if self.matches(e, label)
                         and e.get("accessibleRole") == "Button"
                         and (e.get("accessibleDescription") or "").split(",")[0] == "Current section"), None)
        return wait(f"current section {label}", current)

    def dimensions(self):
        props = self.window_properties()
        return tuple(props["size"][key] / props["scaleFactor"] for key in ("width", "height"))

    def click_element(self, element):
        # MCP synthesizes a left press/release at the item's center in this
        # window, including popup offsets. Never invoke accessible default.
        x, y, x2, y2 = bounds(element)
        check(x2 > x and y2 > y, f"empty pointer target: {element.get('accessibleLabel')}")
        self.mcp.call("click_element", {"elementHandle": element["handle"]})

    def click(self, pattern):
        element = self.find(pattern, roles=("Button", "Checkbox"))
        check(element.get("accessibleEnabled", True), f"native control disabled: {pattern}")
        self.click_element(element)

    def pointer_position_for_click(self, label, *, element=None):
        # Record the actual client-window press, even for popup-local bounds
        # or a non-accessible item such as a playlist icon or subtitle.
        self.mcp.call("start_event_recording", {})
        try:
            if element is None:
                self.click(label)
            else:
                self.click_element(element)
        finally:
            events = self.mcp.value("stop_event_recording", {})
        check(not events.get("droppedCount") and not events.get("unknownEventCount"),
              "incomplete native pointer event recording")
        presses = [item["event"]["pointerPressed"]["position"] for item in events["events"]
                   if "pointerPressed" in item["event"] and item["windowHandle"] == self.handle]
        check(len(presses) == 1, f"expected one native pointer press for {label}")
        return presses[0]["x"], presses[0]["y"]

    def key(self, text, event="PressAndRelease"):
        self.mcp.call("dispatch_key_event", {"windowHandle": self.handle, "text": text, "eventType": event})

    def shortcut(self, modifier, key):
        self.key(modifier, "Press")
        try:
            self.key(key)
        finally:
            self.key(modifier, "Release")

    def fill(self, pattern, value):
        self.click_element(self.find(pattern, roles=("TextInput",)))
        # Type through the real input rather than assigning a model property.
        self.key(value)

    def title(self, name, timeout=10):
        return self.find(re.escape(name), timeout=timeout, roles=("Text",))

    def scroll_catalog(self, delta):
        width = self.window_properties()["size"]["width"] / self.properties["scaleFactor"]
        self.mcp.call("dispatch_pointer_scroll", {"windowHandle": self.handle,
            "position": {"x": width / 2, "y": 350}, "deltaX": 0, "deltaY": delta})

    def drag_slider(self, fraction):
        slider = self.find("Playback position", roles=("Slider",))
        check(slider.get("accessibleEnabled"), "playback slider is disabled")
        # Drag the visible rail, not a model value or the hidden Slider's center.
        rail = wait("rendered seek rail", lambda: element_type(self.elements(), "SeekBar"), 3)
        x, y, right, bottom = bounds(rail)
        check(right - x >= 32 and bottom > y, f"rendered seek rail has no usable pointer target: {bounds(rail)}")
        self.mcp.call("drag_element", {"elementHandle": rail["handle"],
            "target": {"x": x + (right - x) * fraction, "y": (y + bottom) / 2}})

    def pixels(self, name, expected_width=None):
        for element in self.elements():
            check(not (self.matches(element, "Username|Password") and element.get("accessibleValue")),
                  "refusing to capture populated login fields")
        image = next((part for part in self.mcp.call("take_screenshot", {
            "windowHandle": self.handle})["content"] if part["type"] == "image"), None)
        check(image and image["mimeType"] == "image/png", "native screenshot did not return a PNG")
        data = b64decode(image["data"], validate=True)
        check(data[:8] == b"\x89PNG\r\n\x1a\n" and data[12:16] == b"IHDR" and len(data) > 4096,
              "native screenshot was empty or malformed")
        width, height = struct.unpack("!II", data[16:24])
        check(width >= 480 and height >= 440, "unexpected native window pixel dimensions")
        size = self.window_properties()["size"]
        check((width, height) == (size["width"], size["height"]), "PNG and native window dimensions differ")
        if expected_width is not None:
            check(width == expected_width, f"{name}: expected {expected_width} px, got {width} px")
        digest = hashlib.sha256(data).hexdigest()
        check(digest not in self.pictures, f"native frame unchanged at {name}")
        self.pictures.add(digest)
        if self.output:
            with (self.output / (name + ".png")).open("xb") as file:
                file.write(data)
        print(f"PASS native pixels {name}: {width}x{height}", flush=True)


def element_type(elements, name):
    return next((e for e in elements if any(t.get("typeName") == name for t in e.get("typeNamesAndIds", []))), None)


def element_id(elements, name):
    return next((e for e in elements if any(t.get("id", "").split("::")[-1] == name
                for t in e.get("typeNamesAndIds", []))), None)


def bounds(element):
    pos, size = element.get("absolutePosition", {}), element.get("size", {})
    x, y = pos.get("x", 0), pos.get("y", 0)
    return x, y, x + size.get("width", 0), y + size.get("height", 0)


@contextmanager
def verify(failures, label):
    """Continue independent evidence collection; every failed assertion still fails the run."""
    try:
        yield
    except (Failure, TimeoutError) as error:
        failures.append(f"{label}: {error}")
        print(f"FAIL {failures[-1]}", flush=True)


def assert_dock_layout(window):
    elements = window.elements()
    def named(pattern):
        return next((e for e in elements if window.matches(e, pattern) and e.get("accessibleRole") == "Button"), None)
    dock = element_type(elements, "PlayerBar")
    catalog = element_id(elements, "catalog-scroll")
    check(dock and catalog, "missing catalog viewport or dock in the Slint tree")
    left, top, right, bottom = bounds(dock)
    width, height = window.dimensions()
    check(abs(bounds(catalog)[3] - top) <= 1, "catalog viewport and dock do not meet without overlap")
    check(left == 0 and abs(right - width) <= 1 and abs(bottom - height) <= 1 and 48 < bottom - top <= 100,
          f"dock not anchored to client bottom or unexpected height: {bounds(dock)}")
    transport = element_type(window.subtree(dock), "Transport")
    check(transport, "missing dock transport")
    transport_controls = window.subtree(transport)
    for label in ("Previous track", "Play|Pause", "Next track"):
        check(any(window.matches(e, label) and e.get("accessibleRole") == "Button" for e in transport_controls),
              f"missing dock transport control {label}")
    midpoint = sum(bounds(transport)[::2]) / 2
    check(abs(midpoint - width / 2) <= 1, "dock transport is not centered")
    rail = element_type(elements, "SeekBar")
    check(rail, "populated dock must render a seek rail at both widths")
    x, y, x2, y2 = bounds(rail)
    check(left <= x < x2 <= right and bounds(transport)[3] <= y < y2 <= bottom,
          "dock seek rail overlaps transport or leaves the dock")
    previous_right = left
    for label, control in ((DOCK_TITLE, named(DOCK_TITLE)), ("Transport", transport),
                           ("Play queue", named("Play queue")), ("Playback devices", named("Playback devices"))):
        check(control, f"missing dock control {label}")
        x, y, x2, y2 = bounds(control)
        check(previous_right <= x < x2 <= right and top <= y < y2 <= bottom,
              f"dock control {label} is clipped or overlaps another group: {bounds(control)}")
        previous_right = x2
    print(f"PASS dock geometry: {width:g}×{height:g} logical, height {bottom - top:g}, controls disjoint", flush=True)


def full_player(window, title, name, width, failures):
    window.click(title)  # Strict pointer activation: no keyboard or accessibility fallback.
    try:
        wait("full-player page", lambda: element_type(window.elements(), "NowPlaying"), timeout=3)
    except Failure:
        window.pixels(name + "-pointer-failed", width)
        raise
    print(f"PASS {width}px dock identity opens full player by pointer", flush=True)
    window.find("Play queue", roles=("Button",))
    window.find("Playback devices", roles=("Button",))
    window.find(r"Synthetic Tone \d{3}", roles=("Text",))
    window.pixels(name, width)
    with verify(failures, name + " slider hit target"):
        slider = window.find("Playback position", roles=("Slider",))
        x, y, right, bottom = bounds(slider)
        check(right - x >= 32 and bottom > y, f"full-player slider has no usable pointer target: {bounds(slider)}")
    if not any(window.matches(e, "Lyrics") for e in window.elements()):
        window.scroll_catalog(-360)
        window.find("Lyrics", roles=("Text",), timeout=3)
        window.pixels(name + "-details", width)
        window.scroll_catalog(360)


@contextmanager
def desktop(binary, root, env, pixels=None, *, multisize=False):
    cage = shutil.which("cage")
    check(cage, "native acceptance needs cage (headless Wayland only)")
    randr = shutil.which("wlr-randr") if multisize else None
    check(not multisize or randr, "native multisize acceptance needs wlr-randr")
    # The reserved port is closed just before launch. Only connect to the spawned
    # debug process while it is alive, and run this in a private worker if available.
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
    env = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_HEADLESS_OUTPUTS="1",
               SLINT_BACKEND="winit-software", SLINT_MCP_PORT=str(port))
    if "ALSA_CONFIG_PATH" not in env:
        alsa = root / "asound-null.conf"
        alsa.write_text('pcm.!default { type null hint { show on description "Disposable acceptance sink" } }\n')
        env["ALSA_CONFIG_PATH"] = str(alsa)
    check("DISPLAY" not in env and "WAYLAND_DISPLAY" not in env and "WAYLAND_SOCKET" not in env,
          "native acceptance requires a private display environment")
    mcp = mcp_class()(port)
    with (root / "desktop.log").open("w+b") as log:
        process = subprocess.Popen([cage, "--", str(binary)], env=env, cwd=root,
                                   stdout=log, stderr=log, start_new_session=True)
        try:
            def discover():
                if process.poll() is not None:
                    log.seek(0)
                    # No login inputs have been entered at this point.
                    tail = log.read(16384).decode(errors="replace").splitlines()[-5:]
                    raise Failure("owned Wayland compositor/desktop exited before readiness: " + " | ".join(tail))
                try:
                    windows = mcp.value("list_windows", {})["windowHandles"]
                except URLError:
                    return None
                check(len(windows) <= 1, "expected only the owned Slint desktop window")
                return windows[0] if windows else None
            handle = wait("Slint MCP on owned Wayland display", discover, 15)

            def resize_output(mode):
                check(process.poll() is None, "owned Cage not running")
                runtime = Path(env["XDG_RUNTIME_DIR"])
                displays = [path for path in runtime.glob("wayland-*") if path.is_socket()]
                check(len(displays) == 1, "expected exactly one owned Wayland socket")
                display_env = dict(env, WAYLAND_DISPLAY=str(displays[0]))
                listing = subprocess.run([randr], env=display_env, cwd=root, capture_output=True,
                                         text=True, timeout=5)
                check(listing.returncode == 0, "cannot read owned Cage output: " + listing.stderr.strip())
                outputs = re.findall(r"^(HEADLESS-\d+)\s", listing.stdout, re.MULTILINE)
                check(len(outputs) == 1, "expected exactly one owned headless output")
                width, height = mode
                changed = subprocess.run([randr, "--output", outputs[0], "--custom-mode", f"{width}x{height}@60Hz"],
                                         env=display_env, cwd=root, capture_output=True, text=True, timeout=5)
                check(changed.returncode == 0, "cannot resize owned Cage output: " + changed.stderr.strip())

            yield Window(mcp, handle, pixels, resize_output if multisize else None)
        finally:
            # Cage and the desktop are in a fresh owned process group. Stop both
            # even if startup failed before an app window could be discovered.
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                stop(process)
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


REMEMBER_LOGIN = "Keep me signed in"


def login_options(window, *, required=False):
    controls = [e for e in window.elements() if e.get("accessibleRole") == "Checkbox"]
    check(len(controls) <= 1 and all(window.matches(e, REMEMBER_LOGIN) for e in controls),
          "unexpected sign-in checkbox (HTTP consent is not part of sign-in)")
    check(not required or controls, "missing Keep me signed in checkbox")
    if controls:
        option = controls[0]
        x, y, right, bottom = bounds(option)
        width, height = window.dimensions()
        check(0 <= x < right <= width and 0 <= y < bottom <= height,
              "Keep me signed in checkbox is not inside the native window")
        return option


@contextmanager
def native_observer(api):
    # Go pings every five seconds and closes an unresponsive socket. Keep the
    # real read-only peer reading (and replying) while the main thread drives UI.
    with Peer(api) as peer:
        stopping = Event()
        errors = []

        def read_outputs():
            try:
                while not stopping.is_set():
                    peer.wait_frame(lambda _: True, timeout=120)
            except (Failure, OSError, ValueError) as error:
                if not stopping.is_set():
                    errors.append(error)

        reader = Thread(target=read_outputs, name="native-account-observer")
        reader.start()

        def observe(label, predicate, timeout=10):
            def matching_snapshot():
                if errors:
                    raise Failure(f"real Go account observer stopped while waiting for {label}: {errors[0]}")
                snapshot = peer.snapshot
                return snapshot if predicate(snapshot) else None
            return wait(label, matching_snapshot, timeout)

        try:
            yield observe
            check(not errors, f"real Go account observer stopped: {errors[0] if errors else ''}")
        finally:
            stopping.set()
            # Unblock the reader, then let Peer close the owned socket.
            try:
                peer.sock.shutdown(socket.SHUT_RD)
            except OSError:
                pass
            reader.join()


def native_checks(binary, root, env, ready=None, api=None, pixels=None, pcm=None):
    from workspace_checks import (check_navigation, check_shortcut_back, committed_playlist_edits,
                                  compact_queue, empty_devices, keyboard_playlist_menu, manual_options, open_manual,
                                  pane_layout, playlist_back, playlist_hit_checks, playlist_play, playlist_row_menu,
                                  playlists_root, queue_state, root_navigation, rules_editor, typed_catalog,
                                  wide_queue_toggle)

    failures = []
    with desktop(binary, root, env, pixels, multisize=ready is not None) as window:
        if ready is not None:
            window.resize(DESKTOP)
        window.find("Sign in")
        login_options(window)
        window.pixels("01-login-empty")
        if ready is None:
            print("NOT RUN native sign-in/browsing/player (window-only run)", flush=True)
            return
        window.fill("Server( origin| URL| address)?", ready["origin"])
        window.fill("Username", ready["username"])
        window.fill("Password", ready["password"])
        window.click("Sign in")
        window.section("History")
        check_navigation(window)
        pane_layout(window, wide=True)
        window.pixels("02-signed-in", DESKTOP[0])

        if api is None:
            root_navigation(window, wide=True)
            playlist_back(window, wide=True)
            playlist_hit_checks(window)
            rules_editor(window, "03-rules-editor")
            typed_catalog(window, DESKTOP[0], capture=True, via_shortcut=True)
            check_shortcut_back(window)
            open_manual(window)
            manual_options(window, capture="04-playlist-menu-1910")
            keyboard_playlist_menu(window)
            playlist_row_menu(window)
            window.pixels("04-manual-playlist", DESKTOP[0])
            wide_queue_toggle(window, lambda: window.title(PLAYLIST), can_go_back=True)
            window.resize(COMPACT)
            pane_layout(window, wide=False, can_go_back=True)
            compact_queue(window, lambda: window.title(PLAYLIST), capture="05-queue-empty-compact")
            manual_options(window, capture="05-playlist-menu-640")
            window.click("Back")
            window.find(f"Open {PLAYLIST}", roles=("Button",))
            typed_catalog(window, COMPACT[0], capture=True, check_compact_targets=True)
            window.click("Settings")
            window.find("Work offline", roles=("Button",))
            window.find("Reset display defaults", roles=("Button",))
            window.pixels("06-settings-compact", COMPACT[0])
            print("PASS workspace interaction against Go-shaped wire data (no Go playback)", flush=True)
            return
        # Join only after the desktop: an observer must not become the initial
        # selected logical output or change how the desktop is admitted.
        with native_observer(api) as observe:
            def selected_desktop(snapshot):
                return any(device.get("name") == "Spofify Desktop" and device.get("canPlay")
                           and device.get("id") == snapshot.get("owner")
                           for device in snapshot.get("devices", []))

            # The initial output transfer can still be draining while the
            # catalog is already interactive. This case exercises Play on a
            # settled output, not rejection of actions during a handoff.
            observe("desktop selected as the real Go output", selected_desktop, timeout=20)

            def restore_playlists():
                playlists_root(window)

            for mode in (DESKTOP, WIDE, COMPACT):
                if mode != DESKTOP:
                    window.resize(mode)
                    window.click("History")
                wide = mode != COMPACT
                root_navigation(window, wide=wide)
                playlist_back(window, wide=wide)
                playlist_hit_checks(window)
                empty_devices(window, restore_playlists, wide=wide, observer=True,
                              capture="03-devices" if mode == DESKTOP else None)
                if wide:
                    wide_queue_toggle(window, restore_playlists)
                else:
                    compact_queue(window, restore_playlists, capture="05-queue-empty-compact")
            window.resize(DESKTOP)
            open_manual(window)
            manual_options(window)
            window.pixels("03-manual-playlist", DESKTOP[0])
            window.click("Back")
            restore_playlists()

            def canonical_track(snapshot):
                state = snapshot.get("player", {})
                queue, cursor = state.get("queue", []), state.get("cursor", {})
                return (len(queue) > 1 and cursor.get("desired") and any(
                    track.get("key") == cursor.get("key") and track.get("id") == ready["duplicateTrackId"]
                    for track in queue))

            if pcm is not None:
                from audio_smoke import heard_tone
                baseline = pcm.stat().st_size
            # The actual Go fixture clips last one second. Start the full
            # playlist using the row's separate Play target, not its open target.
            playlist_play(window)
            observe("playlist row Play to update the canonical online queue", canonical_track, timeout=20)
            if pcm is not None:
                with verify(failures, "real Go online PCM"):
                    wait("220 Hz Go fixture tone at the private monitor", lambda: heard_tone(pcm, hz=220, start=baseline), 5)
                    print("PASS real Go online track rendered 220 Hz PCM", flush=True)
            window.find(DOCK_TITLE, roles=("Button",))
            window.find("Pause", roles=("Button",))
            restore_playlists()
            window.absent("Back", roles=("Button",))
            print("PASS playlist row Play updates the canonical online queue and dock without opening detail", flush=True)
            open_manual(window)
            for mode in (DESKTOP, WIDE):
                if mode != DESKTOP:
                    window.resize(mode)
                window.title(PLAYLIST)
                pane_layout(window, wide=True, docked=True, can_go_back=True)
                queue_state(window, populated=True)
                window.find("Pause", roles=("Button",))
                window.pixels(f"04-playing-{mode[0]}", mode[0])
                with verify(failures, f"dock geometry at {mode[0]}"):
                    assert_dock_layout(window)
            window.resize(COMPACT)
            window.find(DOCK_TITLE, roles=("Button",))
            pane_layout(window, wide=False, docked=True, can_go_back=True)
            window.find("Pause", roles=("Button",))
            window.pixels("05-dock-compact", COMPACT[0])
            with verify(failures, "compact dock geometry"):
                assert_dock_layout(window)
            window.click("Pause")

            def paused_selection(snapshot):
                player = snapshot["player"]
                cursor = player["cursor"]
                return not cursor["desired"] and not cursor["playing"] and any(
                    cursor["key"] == row["key"] for row in player["queue"])

            observe("native Pause to leave a selected canonical queue occurrence", paused_selection)
            wait("paused dock transport", lambda: not any(window.matches(e, "Pause") for e in window.elements()))
            pane_layout(window, wide=False, docked=True, can_go_back=True)
            assert_dock_layout(window)
            window.pixels("06-dock-paused-compact", COMPACT[0])
            print("PASS canonical paused selection retains the dock and its controls", flush=True)
            compact_queue(window, lambda: window.title(PLAYLIST), populated=True, capture="06-queue-paused-compact")
            full_player(window, DOCK_TITLE, "07-player-compact", COMPACT[0], failures)
            window.click("Back")
            window.title(PLAYLIST)
            window.resize(WIDE)

        album = next(target["name"] for target in ready["targets"] if target["kind"] == "album")
        window.click(f"Open album: {re.escape(album)}")
        window.title(album)
        window.find(f"Open {TRACK}", roles=("Button",))
        window.pixels("09-album-detail", WIDE[0])
        print("PASS native album metadata link opens the real Go catalog detail", flush=True)

        row_menu = rf"(Add to queue|Add to playlist|Save offline) · {TRACK}"
        window.absent(row_menu, roles=("Button",))
        window.click(f"Show actions for {TRACK}")
        for option in ("Add to queue", "Add to playlist", "Save offline"):
            window.find(f"{option} · {TRACK}", roles=("Button",))
        window.absent(r"(?:Like|Unlike)(?: · .*)?", roles=("Button",))
        window.click(f"Save offline · {TRACK}")
        window.title("Saved on this device.")
        window.absent(row_menu, roles=("Button",))
        window.click(SAVED)
        window.section(SAVED)
        window.absent("Back", roles=("Button",))
        window.find(f"Open {TRACK}", roles=("Button",))
        window.absent(OFFLINE_SWITCH, roles=("Button", "Checkbox"))
        window.click("Settings")
        window.title("Settings")
        window.click("Work offline")
        window.section(SAVED)  # Entering offline mode lands on saved music.
        window.title("Offline mode")
        window.absent(OFFLINE_SWITCH, roles=("Button", "Checkbox"))
        window.click("Settings")
        window.find("Go online", roles=("Button",))
        window.pixels("10-offline-settings")
        window.click(SAVED)
        window.section(SAVED)
        window.absent(OFFLINE_SWITCH, roles=("Button", "Checkbox"))
        pane_layout(window, wide=True)
        window.find(f"Play {TRACK}", timeout=15, roles=("Button",))
        window.pixels("11-offline-ready")
        if pcm is not None:
            baseline = pcm.stat().st_size
        window.click("Play Synthetic Tone 001")
        window.find("Now playing: Synthetic Tone 001")
        window.pixels("12-offline-dock")
        if pcm is not None:
            with verify(failures, "real Go saved/offline PCM"):
                wait("220 Hz saved Go fixture tone at the private monitor", lambda: heard_tone(pcm, hz=220, start=baseline), 5)
                print("PASS saved Go track rendered 220 Hz PCM while offline", flush=True)
        print("PASS native save, manual offline switch and saved-audio dock", flush=True)
        full_player(window, f"Now playing: {TRACK}", "13-now-playing-wide", WIDE[0], failures)
        window.click("Back")
        window.section(SAVED)
        window.click("Settings")
        window.click("Go online")
        window.find("Work offline", roles=("Button",))
        window.absent("Offline mode", roles=("Text",))
        print("PASS Settings returns to online mode", flush=True)
        committed_playlist_edits(window, api)
    check(not failures, "; ".join(failures))
